//! Conformance tests for built-in tools.
//!
//! These tests verify that the Rust tool implementations match the
//! behavior of the original TypeScript implementations.

mod common;

use common::TestHarness;
use pi::tools::Tool;
use std::collections::BTreeMap;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(unix)]
struct UnixModeGuard {
    path: PathBuf,
    original: std::fs::Permissions,
}

#[cfg(unix)]
impl UnixModeGuard {
    fn set(path: &Path, mode: u32) -> Self {
        let original = std::fs::metadata(path)
            .expect("stat permission fixture")
            .permissions();
        let mut restricted = original.clone();
        restricted.set_mode(mode);
        std::fs::set_permissions(path, restricted).expect("set permission fixture mode");
        Self {
            path: path.to_path_buf(),
            original,
        }
    }
}

#[cfg(unix)]
impl Drop for UnixModeGuard {
    fn drop(&mut self) {
        if let Err(err) = std::fs::set_permissions(&self.path, self.original.clone()) {
            eprintln!(
                "failed to restore permissions for {}: {err}",
                self.path.display()
            );
        }
    }
}

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
    fn test_read_offset_zero_matches_default_output() {
        asupersync::test_utils::run_test(|| async {
            let temp_dir = tempfile::tempdir().unwrap();
            let test_file = temp_dir.path().join("test.txt");
            std::fs::write(&test_file, "line1\nline2\nline3").unwrap();

            let tool = pi::tools::ReadTool::new(temp_dir.path());
            let default_result = tool
                .execute(
                    "test-id",
                    serde_json::json!({ "path": test_file.to_string_lossy() }),
                    None,
                )
                .await
                .expect("default read should succeed");
            let offset_result = tool
                .execute(
                    "test-id",
                    serde_json::json!({ "path": test_file.to_string_lossy(), "offset": 0 }),
                    None,
                )
                .await
                .expect("offset=0 read should succeed");

            let default_text = get_text_content(&default_result.content);
            let offset_text = get_text_content(&offset_result.content);
            assert_eq!(
                default_text, offset_text,
                "offset=0 should match the default read output"
            );
            assert!(default_result.details.is_none());
            assert!(offset_result.details.is_none());
        });
    }

    #[test]
    fn test_read_large_limit_matches_default_output() {
        asupersync::test_utils::run_test(|| async {
            let temp_dir = tempfile::tempdir().unwrap();
            let test_file = temp_dir.path().join("test.txt");
            std::fs::write(&test_file, "line1\nline2\nline3").unwrap();

            let tool = pi::tools::ReadTool::new(temp_dir.path());
            let default_result = tool
                .execute(
                    "test-id",
                    serde_json::json!({ "path": test_file.to_string_lossy() }),
                    None,
                )
                .await
                .expect("default read should succeed");
            let limit_result = tool
                .execute(
                    "test-id",
                    serde_json::json!({ "path": test_file.to_string_lossy(), "limit": 100 }),
                    None,
                )
                .await
                .expect("large limit read should succeed");

            let default_text = get_text_content(&default_result.content);
            let limit_text = get_text_content(&limit_result.content);
            assert_eq!(
                default_text, limit_text,
                "limit larger than file length should match default output"
            );
            assert!(default_result.details.is_none());
            assert!(limit_result.details.is_none());
        });
    }

    #[test]
    fn test_read_crlf_matches_lf_output() {
        asupersync::test_utils::run_test(|| async {
            let temp_dir = tempfile::tempdir().unwrap();
            let lf_path = temp_dir.path().join("lf.txt");
            let crlf_path = temp_dir.path().join("crlf.txt");
            std::fs::write(&lf_path, "line1\nline2\nline3").unwrap();
            std::fs::write(&crlf_path, "line1\r\nline2\r\nline3").unwrap();

            let tool = pi::tools::ReadTool::new(temp_dir.path());
            let lf_result = tool
                .execute(
                    "test-id",
                    serde_json::json!({ "path": lf_path.to_string_lossy() }),
                    None,
                )
                .await
                .expect("should succeed");
            let crlf_result = tool
                .execute(
                    "test-id",
                    serde_json::json!({ "path": crlf_path.to_string_lossy() }),
                    None,
                )
                .await
                .expect("should succeed");

            let lf_text = get_text_content(&lf_result.content);
            let crlf_text = get_text_content(&crlf_result.content);
            assert_eq!(
                lf_text, crlf_text,
                "CRLF normalization should match LF output"
            );
            assert!(lf_result.details.is_none());
            assert!(crlf_result.details.is_none());
        });
    }

    #[test]
    fn test_read_cr_only_matches_lf_output() {
        asupersync::test_utils::run_test(|| async {
            let temp_dir = tempfile::tempdir().unwrap();
            let lf_path = temp_dir.path().join("lf.txt");
            let cr_path = temp_dir.path().join("cr.txt");
            std::fs::write(&lf_path, "line1\nline2\nline3").unwrap();
            std::fs::write(&cr_path, "line1\rline2\rline3").unwrap();

            let tool = pi::tools::ReadTool::new(temp_dir.path());
            let lf_result = tool
                .execute(
                    "test-id",
                    serde_json::json!({ "path": lf_path.to_string_lossy() }),
                    None,
                )
                .await
                .expect("should succeed");
            let cr_result = tool
                .execute(
                    "test-id",
                    serde_json::json!({ "path": cr_path.to_string_lossy() }),
                    None,
                )
                .await
                .expect("should succeed");

            let lf_text = get_text_content(&lf_result.content);
            let cr_text = get_text_content(&cr_result.content);
            assert_eq!(
                lf_text, cr_text,
                "CR-only normalization should match LF output"
            );
            assert!(lf_result.details.is_none());
            assert!(cr_result.details.is_none());
        });
    }

    #[test]
    fn test_read_hashline_is_deterministic() {
        asupersync::test_utils::run_test(|| async {
            let temp_dir = tempfile::tempdir().unwrap();
            let path = temp_dir.path().join("hashline.txt");
            std::fs::write(&path, "alpha\nbeta\n").unwrap();

            let tool = pi::tools::ReadTool::new(temp_dir.path());
            let input = serde_json::json!({
                "path": path.to_string_lossy(),
                "hashline": true
            });

            let first = tool
                .execute("test-id", input.clone(), None)
                .await
                .expect("first read should succeed");
            let second = tool
                .execute("test-id", input, None)
                .await
                .expect("second read should succeed");

            let first_text = get_text_content(&first.content);
            let second_text = get_text_content(&second.content);
            assert_eq!(
                first_text, second_text,
                "hashline output should be deterministic across reads"
            );
        });
    }

    #[test]
    fn test_read_hashline_subset_matches_full_output() {
        asupersync::test_utils::run_test(|| async {
            let temp_dir = tempfile::tempdir().unwrap();
            let path = temp_dir.path().join("hashline_subset.txt");
            std::fs::write(&path, "alpha\nbeta\ngamma\ndelta\nepsilon\n").unwrap();

            let tool = pi::tools::ReadTool::new(temp_dir.path());
            let full = tool
                .execute(
                    "test-id",
                    serde_json::json!({ "path": path.to_string_lossy(), "hashline": true }),
                    None,
                )
                .await
                .expect("full hashline read should succeed");
            let subset = tool
                .execute(
                    "test-id",
                    serde_json::json!({
                        "path": path.to_string_lossy(),
                        "hashline": true,
                        "offset": 3,
                        "limit": 2
                    }),
                    None,
                )
                .await
                .expect("subset hashline read should succeed");

            let full_text = get_text_content(&full.content);
            let subset_text = get_text_content(&subset.content);
            let full_lines: Vec<&str> = full_text
                .lines()
                .filter(|line| {
                    line.split_once(':')
                        .is_some_and(|(tag, _)| tag.contains('#'))
                })
                .collect();
            let subset_lines: Vec<&str> = subset_text
                .lines()
                .filter(|line| {
                    line.split_once(':')
                        .is_some_and(|(tag, _)| tag.contains('#'))
                })
                .collect();
            let start = 2; // offset 3 => 0-indexed 2
            let end = start + 2;
            assert_eq!(
                subset_lines,
                &full_lines[start..end],
                "subset hashline output should match the corresponding full slice"
            );
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
            let expected_limit = format!(
                "exceeds {} limit",
                format_size(pi::tools::DEFAULT_MAX_BYTES)
            );
            assert!(
                text.contains(&expected_limit),
                "expected limit hint '{expected_limit}', got: {text}"
            );
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
            let _mode_guard = UnixModeGuard::set(&path, 0o000);

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
            assert!(
                message.to_lowercase().contains("permission denied"),
                "read should report PermissionDenied: {message}"
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_read_permission_owner_class_does_not_borrow_other_read_bit() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("read_owner_class_is_exclusive");
            let path = harness.create_file("owner-denied.txt", b"owner class secret");
            let _mode_guard = UnixModeGuard::set(&path, 0o004);

            let tool = pi::tools::ReadTool::new(harness.temp_dir());
            let err = tool
                .execute(
                    "test-id",
                    serde_json::json!({ "path": path.to_string_lossy() }),
                    None,
                )
                .await
                .expect_err("the owner class must not borrow the other-class read bit");
            let message = err.to_string();
            assert!(
                message.to_lowercase().contains("permission denied"),
                "read should reject owner-mode 0 even when other-read is set: {message}"
            );
            assert!(
                !message.contains("owner class secret"),
                "permission failure must not disclose file contents"
            );
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

#[cfg(unix)]
mod file_arguments {
    use super::*;

    #[test]
    fn test_at_file_permission_owner_class_does_not_borrow_other_read_bit() {
        let harness = TestHarness::new("at_file_owner_class_is_exclusive");
        let path = harness.create_file("owner-denied.txt", b"at-file secret payload");
        let _mode_guard = UnixModeGuard::set(&path, 0o004);
        let file_args = vec![path.to_string_lossy().into_owned()];

        let err = pi::tools::process_file_arguments(&file_args, harness.temp_dir(), true)
            .expect_err("@file must enforce the selected owner permission class");
        let message = err.to_string();
        assert!(
            message.to_lowercase().contains("permission denied"),
            "@file should reject owner-mode 0 even when other-read is set: {message}"
        );
        assert!(
            !message.contains("at-file secret payload"),
            "permission failure must not disclose @file contents"
        );
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
            let _mode_guard = UnixModeGuard::set(&dir, 0o500);

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
            assert!(
                message.to_lowercase().contains("permission denied"),
                "write should report PermissionDenied: {message}"
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_write_rejects_parent_without_read_before_atomic_mutation() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("write_parent_without_read");
            let parent = harness.create_dir("write-search-only");
            let path = harness.create_file("write-search-only/target.txt", b"original");
            let mode_guard = UnixModeGuard::set(&parent, 0o300);

            let tool = pi::tools::WriteTool::new(harness.temp_dir());
            let err = tool
                .execute(
                    "test-id",
                    serde_json::json!({
                        "path": path.to_string_lossy(),
                        "content": "replacement"
                    }),
                    None,
                )
                .await
                .expect_err("durable atomic write requires opening the parent directory");
            assert!(err.to_string().to_lowercase().contains("permission denied"));

            drop(mode_guard);
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "original");
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
    fn test_edit_crlf_matches_lf_diff_and_preserves_endings() {
        asupersync::test_utils::run_test(|| async {
            let temp_dir = tempfile::tempdir().unwrap();
            let lf_path = temp_dir.path().join("lf.txt");
            let crlf_path = temp_dir.path().join("crlf.txt");
            std::fs::write(&lf_path, "alpha\nbeta\ngamma").unwrap();
            std::fs::write(&crlf_path, "alpha\r\nbeta\r\ngamma").unwrap();

            let tool = pi::tools::EditTool::new(temp_dir.path());

            let lf_result = tool
                .execute(
                    "test-id",
                    serde_json::json!({
                        "path": lf_path.to_string_lossy(),
                        "oldText": "beta",
                        "newText": "delta"
                    }),
                    None,
                )
                .await
                .expect("should succeed");

            let crlf_result = tool
                .execute(
                    "test-id",
                    serde_json::json!({
                        "path": crlf_path.to_string_lossy(),
                        "oldText": "beta",
                        "newText": "delta"
                    }),
                    None,
                )
                .await
                .expect("should succeed");

            let lf_details = lf_result.details.as_ref().expect("lf details");
            let crlf_details = crlf_result.details.as_ref().expect("crlf details");
            assert_eq!(
                lf_details.get("diff"),
                crlf_details.get("diff"),
                "CRLF diff should match LF diff"
            );
            assert_eq!(
                lf_details.get("firstChangedLine"),
                crlf_details.get("firstChangedLine")
            );

            let lf_content = std::fs::read_to_string(&lf_path).unwrap();
            let crlf_content = std::fs::read_to_string(&crlf_path).unwrap();
            assert!(
                !lf_content.contains('\r'),
                "LF file should not contain carriage returns"
            );
            assert!(
                crlf_content.contains("\r\n"),
                "CRLF file should preserve CRLF endings"
            );
        });
    }

    #[test]
    fn test_edit_cr_only_matches_lf_diff_and_preserves_endings() {
        asupersync::test_utils::run_test(|| async {
            let temp_dir = tempfile::tempdir().unwrap();
            let lf_path = temp_dir.path().join("lf.txt");
            let cr_path = temp_dir.path().join("cr.txt");
            std::fs::write(&lf_path, "alpha\nbeta\ngamma").unwrap();
            std::fs::write(&cr_path, "alpha\rbeta\rgamma").unwrap();

            let tool = pi::tools::EditTool::new(temp_dir.path());

            let lf_result = tool
                .execute(
                    "test-id",
                    serde_json::json!({
                        "path": lf_path.to_string_lossy(),
                        "oldText": "beta",
                        "newText": "delta"
                    }),
                    None,
                )
                .await
                .expect("should succeed");

            let cr_result = tool
                .execute(
                    "test-id",
                    serde_json::json!({
                        "path": cr_path.to_string_lossy(),
                        "oldText": "beta",
                        "newText": "delta"
                    }),
                    None,
                )
                .await
                .expect("should succeed");

            let lf_details = lf_result.details.as_ref().expect("lf details");
            let cr_details = cr_result.details.as_ref().expect("cr details");
            assert_eq!(
                lf_details.get("diff"),
                cr_details.get("diff"),
                "CR-only diff should match LF diff"
            );
            assert_eq!(
                lf_details.get("firstChangedLine"),
                cr_details.get("firstChangedLine")
            );

            let lf_content = std::fs::read_to_string(&lf_path).unwrap();
            let cr_content = std::fs::read_to_string(&cr_path).unwrap();
            assert!(
                !lf_content.contains('\r'),
                "LF file should not contain carriage returns"
            );
            assert!(
                cr_content.contains('\r') && !cr_content.contains('\n'),
                "CR-only file should preserve CR-only endings"
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
    fn test_edit_directory_reports_error() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("edit_directory_reports_error");
            let dir = harness.create_dir("not_a_file");
            let tool = pi::tools::EditTool::new(harness.temp_dir());
            let input = serde_json::json!({
                "path": dir.to_string_lossy(),
                "oldText": "old",
                "newText": "new"
            });

            let err = tool
                .execute("test-id", input, None)
                .await
                .expect_err("should error");
            let message = err.to_string();
            assert!(message.contains("not a regular file"));
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_edit_permission_denied_is_reported() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("edit_permission_denied_is_reported");
            let path = harness.create_file("locked.txt", b"secret");
            let _mode_guard = UnixModeGuard::set(&path, 0o000);

            let tool = pi::tools::EditTool::new(harness.temp_dir());
            let input = serde_json::json!({
                "path": path.to_string_lossy(),
                "oldText": "secret",
                "newText": "public"
            });

            let err = tool
                .execute("test-id", input, None)
                .await
                .expect_err("should error");
            let message = err.to_string().to_lowercase();
            assert!(
                message.contains("permission denied"),
                "edit should report PermissionDenied: {message}"
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_edit_permission_denied_for_unsearchable_parent_is_reported() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("edit_unsearchable_parent_is_reported");
            let locked_dir = harness.create_dir("locked_parent");
            let path = harness.create_file("locked_parent/target.txt", b"secret");
            let mode_guard = UnixModeGuard::set(&locked_dir, 0o000);

            let tool = pi::tools::EditTool::new(harness.temp_dir());
            let input = serde_json::json!({
                "path": path.to_string_lossy(),
                "oldText": "secret",
                "newText": "public"
            });

            let err = tool
                .execute("test-id", input, None)
                .await
                .expect_err("an unsearchable parent must block editing");
            let message = err.to_string().to_lowercase();
            assert!(
                message.contains("permission denied"),
                "edit should report ancestor PermissionDenied: {message}"
            );

            drop(mode_guard);
            assert_eq!(
                std::fs::read_to_string(&path).expect("read restored fixture"),
                "secret",
                "permission failure must leave the file unchanged"
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_edit_rejects_parent_without_read_before_atomic_mutation() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("edit_parent_without_read");
            let parent = harness.create_dir("write-search-only");
            let path = harness.create_file("write-search-only/target.txt", b"original");
            let mode_guard = UnixModeGuard::set(&parent, 0o300);

            let tool = pi::tools::EditTool::new(harness.temp_dir());
            let err = tool
                .execute(
                    "test-id",
                    serde_json::json!({
                        "path": path.to_string_lossy(),
                        "oldText": "original",
                        "newText": "replacement"
                    }),
                    None,
                )
                .await
                .expect_err("durable atomic edit requires opening the parent directory");
            assert!(err.to_string().to_lowercase().contains("permission denied"));

            drop(mode_guard);
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "original");
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

            let result = tool
                .execute("test-id", input, None)
                .await
                .expect("timeout should return a tool error output");
            assert!(result.is_error, "timeout must set is_error");
            let message = get_text_content(&result.content);
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
                "command": "yes a | head -c 1200000"
            });

            let result = tool
                .execute("test-id", input, None)
                .await
                .expect("should succeed");

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

            let result = tool
                .execute("test-id", input, None)
                .await
                .expect("non-zero exit should return a tool error output");
            assert!(result.is_error, "non-zero exit must set is_error");
            assert!(get_text_content(&result.content).contains("Command exited with code 42"));
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

    #[cfg(unix)]
    #[test]
    fn test_grep_escapes_control_characters_and_backslashes_in_file_paths() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("grep_escapes_control_path_bytes");
            harness.create_file("line\nbreak\\tab\tname.txt", b"needle\n");

            let tool = pi::tools::GrepTool::new(harness.temp_dir());
            let result = tool
                .execute("test-id", serde_json::json!({ "pattern": "needle" }), None)
                .await
                .expect("grep should render an unambiguous single-line path");
            let text = get_text_content(&result.content);

            assert_eq!(text, "line\\nbreak\\\\tab\\tname.txt:1: needle");
            assert_eq!(text.lines().count(), 1);
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_grep_renders_invalid_utf8_filename_losslessly() {
        use std::os::unix::ffi::OsStringExt as _;

        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("grep_invalid_utf8_filename");
            let name = std::ffi::OsString::from_vec(b"cafe-\xe2\x98\x83-\xff.txt".to_vec());
            std::fs::write(harness.temp_dir().join(name), b"needle\n")
                .expect("create invalid UTF-8 grep fixture");

            let tool = pi::tools::GrepTool::new(harness.temp_dir());
            let result = tool
                .execute("test-id", serde_json::json!({ "pattern": "needle" }), None)
                .await
                .expect("grep must decode ripgrep path.bytes without data loss");
            let text = get_text_content(&result.content);

            assert_eq!(text, "cafe-☃-\\xFF.txt:1: needle");
            assert_eq!(text.lines().count(), 1);
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_grep_sanitizes_control_bearing_external_diagnostic() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("grep_control_bearing_diagnostic");
            let tool = pi::tools::GrepTool::new(harness.temp_dir());
            let error = tool
                .execute(
                    "test-id",
                    serde_json::json!({ "pattern": "(\n\u{1b}[31m" }),
                    None,
                )
                .await
                .expect_err("invalid regex should expose a sanitized ripgrep diagnostic");
            let message = error.to_string();

            assert_eq!(message.lines().count(), 1, "diagnostic: {message:?}");
            assert!(!message.contains('\u{1b}'), "diagnostic: {message:?}");
            assert!(message.contains("\\n"), "diagnostic: {message:?}");
            assert!(
                message.to_lowercase().contains("regex parse error"),
                "diagnostic: {message:?}"
            );
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

    #[cfg(unix)]
    #[test]
    fn test_grep_permission_denied_is_reported() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("grep_permission_denied_is_reported");
            let dir = harness.create_dir("locked");
            harness.create_file("locked/secret.txt", b"needle\n");
            let _mode_guard = UnixModeGuard::set(&dir, 0o000);

            let tool = pi::tools::GrepTool::new(harness.temp_dir());
            let input = serde_json::json!({
                "pattern": "needle",
                "path": dir.to_string_lossy()
            });

            let err = tool
                .execute("test-id", input, None)
                .await
                .expect_err("mode-000 search root should be rejected");
            let message = err.to_string();
            assert!(message.contains("Tool error: grep:"));
            assert!(
                message.to_lowercase().contains("permission denied"),
                "grep should report PermissionDenied: {message}"
            );
            assert!(
                !message.contains("needle"),
                "permission failure must not disclose scanned content"
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_grep_permission_denied_for_nested_mode_zero_directory_before_scanning() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("grep_nested_mode_zero_directory");
            harness.create_file("public.txt", b"public needle\n");
            let locked = harness.create_dir("buried-vault");
            harness.create_file("buried-vault/denied-name.txt", b"denied needle payload\n");
            let _mode_guard = UnixModeGuard::set(&locked, 0o000);

            let tool = pi::tools::GrepTool::new(harness.temp_dir());
            let err = tool
                .execute(
                    "test-id",
                    serde_json::json!({ "pattern": "needle", "limit": 1 }),
                    None,
                )
                .await
                .expect_err("a nested mode-000 directory must fail closed before rg starts");
            let message = err.to_string();
            assert!(message.to_lowercase().contains("permission denied"));
            assert!(!message.contains("buried-vault"));
            assert!(!message.contains("denied-name.txt"));
            assert!(!message.contains("denied needle payload"));
            assert!(!message.contains("unable to read"));
            assert!(!message.contains("limit reached"));
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_grep_permission_denied_for_nested_owner_file_before_scanning() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("grep_nested_owner_denied_file");
            harness.create_file("public.txt", b"public needle\n");
            let denied = harness.create_file("sealed-result.txt", b"sealed needle payload\n");
            let _mode_guard = UnixModeGuard::set(&denied, 0o004);

            let tool = pi::tools::GrepTool::new(harness.temp_dir());
            let err = tool
                .execute(
                    "test-id",
                    serde_json::json!({ "pattern": "needle", "glob": "*.txt", "limit": 1 }),
                    None,
                )
                .await
                .expect_err("the owner class must not borrow other-read during recursive grep");
            let message = err.to_string();
            assert!(message.to_lowercase().contains("permission denied"));
            assert!(!message.contains("sealed-result.txt"));
            assert!(!message.contains("sealed needle payload"));
            assert!(!message.contains("unable to read"));
            assert!(!message.contains("limit reached"));
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_grep_file_input_does_not_read_directory_ignore_controls() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("grep_file_ignores_directory_controls");
            let target = harness.create_file("direct.txt", b"public needle\n");
            let control = harness.create_file(".gitignore", b"direct.txt\n");
            let _mode_guard = UnixModeGuard::set(&control, 0o000);

            let tool = pi::tools::GrepTool::new(harness.temp_dir());
            let result = tool
                .execute(
                    "test-id",
                    serde_json::json!({
                        "pattern": "needle",
                        "path": target.to_string_lossy()
                    }),
                    None,
                )
                .await
                .expect("an explicit file operand bypasses directory ignore filtering");
            let text = get_text_content(&result.content);
            assert!(text.contains("public needle"), "grep output: {text}");
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_grep_permission_preflight_skips_gitignored_denied_descendants() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("grep_skips_gitignored_denied_descendants");
            harness.create_file(".gitignore", b"ignored-vault/\nignored-owner-denied.txt\n");
            harness.create_file("public.txt", b"public needle\n");
            let locked_dir = harness.create_dir("ignored-vault");
            harness.create_file("ignored-vault/secret.txt", b"ignored needle\n");
            let locked_file = harness.create_file("ignored-owner-denied.txt", b"ignored needle\n");
            let _dir_guard = UnixModeGuard::set(&locked_dir, 0o000);
            let _file_guard = UnixModeGuard::set(&locked_file, 0o004);

            let tool = pi::tools::GrepTool::new(harness.temp_dir());
            let result = tool
                .execute("test-id", serde_json::json!({ "pattern": "needle" }), None)
                .await
                .expect("gitignored denied descendants are outside rg's scan surface");
            let text = get_text_content(&result.content);
            assert!(text.contains("public.txt"), "grep output: {text}");
            assert!(!text.contains("ignored-vault"));
            assert!(!text.contains("ignored-owner-denied.txt"));
            assert!(!text.contains("ignored needle"));
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_grep_permission_preflight_skips_rgignored_denied_descendants() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("grep_skips_rgignored_denied_descendants");
            harness.create_file(".rgignore", b"ignored-vault/\n");
            harness.create_file("public.txt", b"public needle\n");
            let locked = harness.create_dir("ignored-vault");
            harness.create_file("ignored-vault/secret.txt", b"ignored needle\n");
            let _mode_guard = UnixModeGuard::set(&locked, 0o000);

            let tool = pi::tools::GrepTool::new(harness.temp_dir());
            let result = tool
                .execute("test-id", serde_json::json!({ "pattern": "needle" }), None)
                .await
                .expect(".rgignore must define the same preflight and scanner surface");
            let text = get_text_content(&result.content);
            assert!(text.contains("public.txt"), "grep output: {text}");
            assert!(!text.contains("ignored-vault"));
            assert!(!text.contains("secret.txt"));
            assert!(!text.contains("ignored needle"));
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_grep_nested_gitignore_matches_preflight_outside_git_repository() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("grep_nested_gitignore_outside_repository");
            harness.create_file("nested/.gitignore", b"ignored-vault/\n");
            harness.create_file("nested/public.txt", b"public needle\n");
            let locked = harness.create_dir("nested/ignored-vault");
            harness.create_file("nested/ignored-vault/secret.txt", b"ignored needle\n");
            let _mode_guard = UnixModeGuard::set(&locked, 0o000);

            let tool = pi::tools::GrepTool::new(harness.temp_dir());
            let result = tool
                .execute("test-id", serde_json::json!({ "pattern": "needle" }), None)
                .await
                .expect("nested .gitignore semantics must match outside a git repository");
            let text = get_text_content(&result.content);
            assert!(text.contains("nested/public.txt"), "grep output: {text}");
            assert!(!text.contains("ignored-vault"));
            assert!(!text.contains("secret.txt"));
            assert!(!text.contains("ignored needle"));
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
    fn test_grep_zero_limit_rejected() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("grep_zero_limit_rejected");
            harness.create_file("sample.txt", b"alpha\nbeta\n");
            let tool = pi::tools::GrepTool::new(harness.temp_dir());
            let input = serde_json::json!({
                "pattern": "alpha",
                "limit": 0
            });

            let err = tool
                .execute("test-id", input, None)
                .await
                .expect_err("should error");
            let message = err.to_string();
            assert!(
                message.contains("`limit` must be greater than 0"),
                "unexpected error message: {message}"
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
    fn test_grep_literal_matches_plain_pattern_equivalence() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("grep_literal_matches_plain_pattern_equivalence");
            harness.create_file("sample.txt", b"alpha\nbeta\nalpha gamma\n");
            let tool = pi::tools::GrepTool::new(harness.temp_dir());

            let base = tool
                .execute("test-id", serde_json::json!({ "pattern": "alpha" }), None)
                .await
                .expect("should succeed");
            let literal = tool
                .execute(
                    "test-id",
                    serde_json::json!({ "pattern": "alpha", "literal": true }),
                    None,
                )
                .await
                .expect("should succeed");

            let base_text = get_text_content(&base.content);
            let literal_text = get_text_content(&literal.content);
            assert_eq!(
                base_text, literal_text,
                "literal mode should match regex mode when pattern has no metacharacters"
            );
        });
    }

    #[test]
    fn test_grep_ignore_case_is_noop_for_lowercase_content() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("grep_ignore_case_is_noop_for_lowercase_content");
            harness.create_file("sample.txt", b"alpha\nbeta\nalpha gamma\n");
            let tool = pi::tools::GrepTool::new(harness.temp_dir());

            let base = tool
                .execute(
                    "test-id",
                    serde_json::json!({ "pattern": "alpha", "ignoreCase": false }),
                    None,
                )
                .await
                .expect("should succeed");
            let insensitive = tool
                .execute(
                    "test-id",
                    serde_json::json!({ "pattern": "alpha", "ignoreCase": true }),
                    None,
                )
                .await
                .expect("should succeed");

            let base_text = get_text_content(&base.content);
            let insensitive_text = get_text_content(&insensitive.content);
            assert_eq!(
                base_text, insensitive_text,
                "ignoreCase should not change results when content is already lowercase"
            );
        });
    }

    #[test]
    fn test_grep_hashline_tags_match_read_output() {
        asupersync::test_utils::run_test(|| async {
            let temp_dir = tempfile::tempdir().unwrap();
            let path = temp_dir.path().join("hashline_grep.txt");
            std::fs::write(&path, "alpha\nneedle line\nomega").unwrap();

            let read_tool = pi::tools::ReadTool::new(temp_dir.path());
            let read_out = read_tool
                .execute(
                    "test-id",
                    serde_json::json!({ "path": path.to_string_lossy(), "hashline": true }),
                    None,
                )
                .await
                .expect("hashline read should succeed");
            let read_text = get_text_content(&read_out.content);
            let read_tag = read_text
                .lines()
                .find(|line| line.starts_with("2#"))
                .and_then(|line| line.split_once(':'))
                .map(|(tag, _)| tag.to_string())
                .expect("expected hashline tag for line 2");

            let grep_tool = pi::tools::GrepTool::new(temp_dir.path());
            let grep_out = grep_tool
                .execute(
                    "test-id",
                    serde_json::json!({
                        "pattern": "needle",
                        "path": path.to_string_lossy(),
                        "hashline": true
                    }),
                    None,
                )
                .await
                .expect("hashline grep should succeed");
            let grep_text = get_text_content(&grep_out.content);
            let mut parts = grep_text.splitn(3, ':');
            let _file = parts.next().unwrap_or_default();
            let grep_tag = parts.next().unwrap_or_default().trim().to_string();

            assert_eq!(
                grep_tag, read_tag,
                "grep hashline tag should match read hashline tag for the same line"
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

    #[cfg(unix)]
    #[test]
    fn test_find_permission_denied_is_reported() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("find_permission_denied_is_reported");
            let dir = harness.create_dir("locked");
            let _mode_guard = UnixModeGuard::set(&dir, 0o000);
            let tool = pi::tools::FindTool::new(harness.temp_dir());
            let input = serde_json::json!({
                "pattern": "*.txt",
                "path": dir.to_string_lossy()
            });

            let err = tool
                .execute("test-id", input, None)
                .await
                .expect_err("mode-000 search root should be rejected");
            let message = err.to_string();
            assert!(message.contains("Tool error: find:"));
            assert!(
                message.to_lowercase().contains("permission denied"),
                "find should report PermissionDenied: {message}"
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_find_permission_denied_for_nested_mode_zero_directory_before_scanning() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("find_nested_mode_zero_directory");
            harness.create_file("public.txt", b"");
            let locked = harness.create_dir("buried-vault");
            harness.create_file("buried-vault/denied-name.txt", b"");
            let _mode_guard = UnixModeGuard::set(&locked, 0o000);

            let tool = pi::tools::FindTool::new(harness.temp_dir());
            let err = tool
                .execute(
                    "test-id",
                    serde_json::json!({ "pattern": "*.txt", "limit": 1 }),
                    None,
                )
                .await
                .expect_err("a nested mode-000 directory must fail closed before fd starts");
            let message = err.to_string();
            assert!(message.to_lowercase().contains("permission denied"));
            assert!(!message.contains("buried-vault"));
            assert!(!message.contains("denied-name.txt"));
            assert!(!message.contains("limit reached"));
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_find_permission_policy_allows_owner_unreadable_regular_file_names() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("find_unreadable_regular_file_name");
            let unreadable = harness.create_file("visible-name.txt", b"unreadable payload");
            let _mode_guard = UnixModeGuard::set(&unreadable, 0o004);

            let tool = pi::tools::FindTool::new(harness.temp_dir());
            let result = tool
                .execute("test-id", serde_json::json!({ "pattern": "*.txt" }), None)
                .await
                .expect("find only enumerates the directory entry name");
            let text = get_text_content(&result.content);
            assert!(text.contains("visible-name.txt"), "find output: {text}");
            assert!(!text.contains("unreadable payload"));
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_find_no_follow_does_not_classify_symlink_by_target_metadata() {
        use std::os::unix::fs::symlink;

        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("find_no_follow_symlink_metadata");
            let outside = tempfile::tempdir().expect("outside target directory");
            let _mode_guard = UnixModeGuard::set(outside.path(), 0o000);
            symlink(outside.path(), harness.temp_dir().join("outside-link"))
                .expect("create directory symlink");

            let tool = pi::tools::FindTool::new(harness.temp_dir());
            let result = tool
                .execute(
                    "test-id",
                    serde_json::json!({ "pattern": "outside-link" }),
                    None,
                )
                .await
                .expect("--no-follow find should inspect only the symlink entry");
            let text = get_text_content(&result.content);
            assert!(text.lines().any(|line| line == "outside-link"), "{text}");
            assert!(
                !text.lines().any(|line| line == "outside-link/"),
                "a no-follow symlink must not be labeled from its target type: {text}"
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_find_preserves_leading_and_trailing_spaces_in_filename() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("find_preserves_filename_spaces");
            harness.create_file(" leading-and-trailing.txt ", b"");

            let tool = pi::tools::FindTool::new(harness.temp_dir());
            let result = tool
                .execute("test-id", serde_json::json!({ "pattern": "*" }), None)
                .await
                .expect("valid filename whitespace must survive fd output parsing");
            let text = get_text_content(&result.content);

            assert_eq!(text, " leading-and-trailing.txt ");
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_find_newline_in_directory_name_cannot_create_fake_parent_path() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("find_nul_delimited_paths");
            let fixture_parent = harness
                .temp_dir()
                .parent()
                .expect("harness temp directory has a parent");
            let outside = tempfile::tempdir_in(fixture_parent).expect("outside sibling directory");
            let outside_name = outside
                .path()
                .file_name()
                .expect("outside directory has a basename")
                .to_string_lossy();

            harness.create_dir("prefix\n..");
            harness.create_file(format!("prefix\n../{outside_name}"), b"inside entry");

            let tool = pi::tools::FindTool::new(harness.temp_dir());
            let result = tool
                .execute(
                    "test-id",
                    serde_json::json!({ "pattern": outside_name.as_ref() }),
                    None,
                )
                .await
                .expect("newline-bearing path must stay one fd output record");
            let text = get_text_content(&result.content);
            let expected = format!("prefix\\n../{outside_name}");

            assert_eq!(text, expected, "find must emit one escaped result line");
            assert!(
                !text.contains(&format!("../{outside_name}/")),
                "a split output record must not stat and classify the outside sibling: {text:?}"
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_find_renders_invalid_utf8_filename_losslessly() {
        use std::os::unix::ffi::OsStringExt as _;

        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("find_invalid_utf8_filename");
            let name = std::ffi::OsString::from_vec(b"cafe-\xe2\x98\x83-\xfe.txt".to_vec());
            std::fs::write(harness.temp_dir().join(name), b"")
                .expect("create invalid UTF-8 find fixture");

            let tool = pi::tools::FindTool::new(harness.temp_dir());
            let result = tool
                .execute("test-id", serde_json::json!({ "pattern": "*" }), None)
                .await
                .expect("find must retain raw NUL-delimited path bytes");
            let text = get_text_content(&result.content);

            assert_eq!(text, "cafe-☃-\\xFE.txt");
            assert_eq!(text.lines().count(), 1);
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_find_sanitizes_control_bearing_external_diagnostic() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("find_control_bearing_diagnostic");
            let tool = pi::tools::FindTool::new(harness.temp_dir());
            let error = tool
                .execute(
                    "test-id",
                    serde_json::json!({ "pattern": "[\n\u{1b}[31m" }),
                    None,
                )
                .await
                .expect_err("invalid glob should expose a sanitized fd diagnostic");
            let message = error.to_string();

            assert_eq!(message.lines().count(), 1, "diagnostic: {message:?}");
            assert!(!message.contains('\u{1b}'), "diagnostic: {message:?}");
            assert!(message.contains("\\n"), "diagnostic: {message:?}");
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_find_permission_preflight_reads_only_consumed_ignore_controls() {
        asupersync::test_utils::run_test(|| async {
            for control in [".gitignore", ".ignore", ".fdignore"] {
                let harness = TestHarness::new(format!(
                    "find_owner_denied_{}",
                    control.trim_start_matches('.')
                ));
                harness.create_file("visible-name.txt", b"ordinary file payload");
                let denied_control = harness.create_file(control, b"ignored-name.txt\n");
                let _mode_guard = UnixModeGuard::set(&denied_control, 0o004);

                let tool = pi::tools::FindTool::new(harness.temp_dir());
                let error = tool
                    .execute("test-id", serde_json::json!({ "pattern": "*.txt" }), None)
                    .await
                    .expect_err("find must read each ignore control before invoking fd");
                let message = error.to_string();
                assert!(
                    message.to_lowercase().contains("permission denied"),
                    "owner-denied {control} must produce PermissionDenied: {message}"
                );
                assert!(
                    !message.contains("ordinary file payload"),
                    "denial must not expose unrelated file contents: {message}"
                );
            }
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_find_permission_preflight_skips_gitignored_denied_directory() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("find_skips_gitignored_denied_directory");
            harness.create_file(".gitignore", b"ignored-vault/\n");
            harness.create_file("public.txt", b"");
            let locked = harness.create_dir("ignored-vault");
            harness.create_file("ignored-vault/secret.txt", b"");
            let _mode_guard = UnixModeGuard::set(&locked, 0o000);

            let tool = pi::tools::FindTool::new(harness.temp_dir());
            let result = tool
                .execute("test-id", serde_json::json!({ "pattern": "*.txt" }), None)
                .await
                .expect("gitignored denied directories are outside fd's traversal surface");
            let text = get_text_content(&result.content);
            assert!(text.contains("public.txt"), "find output: {text}");
            assert!(!text.contains("ignored-vault"));
            assert!(!text.contains("secret.txt"));
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_find_permission_preflight_skips_fdignored_denied_directory() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("find_skips_fdignored_denied_directory");
            harness.create_file(".fdignore", b"ignored-vault/\n");
            harness.create_file("public.txt", b"");
            let locked = harness.create_dir("ignored-vault");
            harness.create_file("ignored-vault/secret.txt", b"");
            let _mode_guard = UnixModeGuard::set(&locked, 0o000);

            let tool = pi::tools::FindTool::new(harness.temp_dir());
            let result = tool
                .execute("test-id", serde_json::json!({ "pattern": "*.txt" }), None)
                .await
                .expect(".fdignore must define the same preflight and scanner surface");
            let text = get_text_content(&result.content);
            assert!(text.contains("public.txt"), "find output: {text}");
            assert!(!text.contains("ignored-vault"));
            assert!(!text.contains("secret.txt"));
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_find_nested_gitignore_matches_preflight_outside_git_repository() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("find_nested_gitignore_outside_repository");
            harness.create_file("nested/.gitignore", b"ignored-vault/\n");
            harness.create_file("nested/public.txt", b"");
            let locked = harness.create_dir("nested/ignored-vault");
            harness.create_file("nested/ignored-vault/secret.txt", b"");
            let _mode_guard = UnixModeGuard::set(&locked, 0o000);

            let tool = pi::tools::FindTool::new(harness.temp_dir());
            let result = tool
                .execute("test-id", serde_json::json!({ "pattern": "*.txt" }), None)
                .await
                .expect("nested .gitignore semantics must match outside a git repository");
            let text = get_text_content(&result.content);
            assert!(text.contains("nested/public.txt"), "find output: {text}");
            assert!(!text.contains("ignored-vault"));
            assert!(!text.contains("secret.txt"));
        });
    }

    #[test]
    fn test_find_rejects_file_search_root() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("find_rejects_file_search_root");
            let file = harness.create_file("not-a-directory.txt", b"");
            let tool = pi::tools::FindTool::new(harness.temp_dir());
            let err = tool
                .execute(
                    "test-id",
                    serde_json::json!({
                        "pattern": "*.txt",
                        "path": file.to_string_lossy()
                    }),
                    None,
                )
                .await
                .expect_err("find path is documented as a directory");
            assert!(err.to_string().contains("Not a directory"));
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

    #[test]
    fn test_find_default_path_equivalence() {
        asupersync::test_utils::run_test(|| async {
            let temp_dir = tempfile::tempdir().unwrap();
            std::fs::write(temp_dir.path().join("alpha.txt"), "").unwrap();
            std::fs::write(temp_dir.path().join("beta.txt"), "").unwrap();

            let tool = pi::tools::FindTool::new(temp_dir.path());
            let base = tool
                .execute("test-id", serde_json::json!({ "pattern": "*.txt" }), None)
                .await
                .expect("should succeed");
            let explicit = tool
                .execute(
                    "test-id",
                    serde_json::json!({ "pattern": "*.txt", "path": "." }),
                    None,
                )
                .await
                .expect("should succeed");

            let mut base_lines = get_text_content(&base.content)
                .lines()
                .map(str::to_string)
                .collect::<Vec<_>>();
            let mut explicit_lines = get_text_content(&explicit.content)
                .lines()
                .map(str::to_string)
                .collect::<Vec<_>>();
            base_lines.sort();
            explicit_lines.sort();
            assert_eq!(
                base_lines, explicit_lines,
                "find results should match when path is omitted vs explicit '.'"
            );
        });
    }

    #[test]
    fn test_find_trailing_slash_equivalence() {
        asupersync::test_utils::run_test(|| async {
            let temp_dir = tempfile::tempdir().unwrap();
            let subdir = temp_dir.path().join("subdir");
            std::fs::create_dir(&subdir).unwrap();
            std::fs::write(subdir.join("alpha.txt"), "").unwrap();
            std::fs::create_dir(subdir.join("nested")).unwrap();
            std::fs::write(subdir.join("nested/beta.txt"), "").unwrap();

            let tool = pi::tools::FindTool::new(temp_dir.path());
            let base = tool
                .execute(
                    "test-id",
                    serde_json::json!({ "pattern": "**/*.txt", "path": "subdir" }),
                    None,
                )
                .await
                .expect("should succeed");
            let explicit = tool
                .execute(
                    "test-id",
                    serde_json::json!({ "pattern": "**/*.txt", "path": "subdir/" }),
                    None,
                )
                .await
                .expect("should succeed");

            let mut base_lines = get_text_content(&base.content)
                .lines()
                .map(str::to_string)
                .collect::<Vec<_>>();
            let mut explicit_lines = get_text_content(&explicit.content)
                .lines()
                .map(str::to_string)
                .collect::<Vec<_>>();
            base_lines.sort();
            explicit_lines.sort();
            assert_eq!(
                base_lines, explicit_lines,
                "find results should match for 'subdir' vs 'subdir/'"
            );
        });
    }

    #[test]
    fn test_find_nonmatching_files_do_not_change_results() {
        asupersync::test_utils::run_test(|| async {
            let temp_dir = tempfile::tempdir().unwrap();
            std::fs::write(temp_dir.path().join("match.rs"), "").unwrap();
            let tool = pi::tools::FindTool::new(temp_dir.path());

            let base = tool
                .execute("test-id", serde_json::json!({ "pattern": "*.rs" }), None)
                .await
                .expect("should succeed");

            std::fs::write(temp_dir.path().join("note.txt"), "").unwrap();

            let after = tool
                .execute("test-id", serde_json::json!({ "pattern": "*.rs" }), None)
                .await
                .expect("should succeed");

            let mut base_lines = get_text_content(&base.content)
                .lines()
                .map(str::to_string)
                .collect::<Vec<_>>();
            let mut after_lines = get_text_content(&after.content)
                .lines()
                .map(str::to_string)
                .collect::<Vec<_>>();
            base_lines.sort();
            after_lines.sort();
            assert_eq!(
                base_lines, after_lines,
                "non-matching files should not affect find results"
            );
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

    #[cfg(unix)]
    #[test]
    fn test_ls_escapes_control_characters_and_backslashes_in_entry_names() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("ls_escapes_control_path_bytes");
            harness.create_file("line\nbreak\\tab\tname.txt", b"");

            let tool = pi::tools::LsTool::new(harness.temp_dir());
            let result = tool
                .execute("test-id", serde_json::json!({}), None)
                .await
                .expect("ls should render an unambiguous single-line path");
            let text = get_text_content(&result.content);

            assert_eq!(text, "line\\nbreak\\\\tab\\tname.txt");
            assert_eq!(text.lines().count(), 1);
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_ls_renders_invalid_utf8_filenames_losslessly_and_deterministically() {
        use std::os::unix::ffi::OsStringExt as _;

        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("ls_invalid_utf8_filenames");
            for bytes in [b"same-\xff.txt".as_slice(), b"same-\xfe.txt".as_slice()] {
                let name = std::ffi::OsString::from_vec(bytes.to_vec());
                std::fs::write(harness.temp_dir().join(name), b"")
                    .expect("create invalid UTF-8 ls fixture");
            }

            let tool = pi::tools::LsTool::new(harness.temp_dir());
            let result = tool
                .execute("test-id", serde_json::json!({}), None)
                .await
                .expect("ls must retain raw OsString names");
            let text = get_text_content(&result.content);

            assert_eq!(text, "same-\\xFE.txt\nsame-\\xFF.txt");
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_ls_escapes_control_bearing_failure_path() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("ls_control_bearing_failure_path");
            let tool = pi::tools::LsTool::new(harness.temp_dir());
            let error = tool
                .execute(
                    "test-id",
                    serde_json::json!({ "path": "missing\n\u{1b}[31m-dir" }),
                    None,
                )
                .await
                .expect_err("missing path should be rendered as one safe diagnostic line");
            let message = error.to_string();

            assert_eq!(message.lines().count(), 1, "diagnostic: {message:?}");
            assert!(!message.contains('\u{1b}'), "diagnostic: {message:?}");
            assert!(message.contains("missing\\n\\u{1b}[31m-dir"));
        });
    }

    #[test]
    fn test_ls_default_path_equivalence() {
        asupersync::test_utils::run_test(|| async {
            let temp_dir = tempfile::tempdir().unwrap();
            std::fs::write(temp_dir.path().join("alpha.txt"), "content").unwrap();
            std::fs::create_dir(temp_dir.path().join("beta")).unwrap();

            let tool = pi::tools::LsTool::new(temp_dir.path());
            let base = tool
                .execute("test-id", serde_json::json!({}), None)
                .await
                .expect("should succeed");
            let explicit = tool
                .execute("test-id", serde_json::json!({ "path": "." }), None)
                .await
                .expect("should succeed");

            let mut base_lines = get_text_content(&base.content)
                .lines()
                .map(str::to_string)
                .collect::<Vec<_>>();
            let mut explicit_lines = get_text_content(&explicit.content)
                .lines()
                .map(str::to_string)
                .collect::<Vec<_>>();
            base_lines.sort();
            explicit_lines.sort();
            assert_eq!(
                base_lines, explicit_lines,
                "ls results should match when path is omitted vs explicit '.'"
            );
        });
    }

    #[test]
    fn test_ls_trailing_slash_equivalence() {
        asupersync::test_utils::run_test(|| async {
            let temp_dir = tempfile::tempdir().unwrap();
            let subdir = temp_dir.path().join("subdir");
            std::fs::create_dir(&subdir).unwrap();
            std::fs::write(subdir.join("alpha.txt"), "content").unwrap();
            std::fs::create_dir(subdir.join("nested")).unwrap();

            let tool = pi::tools::LsTool::new(temp_dir.path());
            let base = tool
                .execute("test-id", serde_json::json!({ "path": "subdir" }), None)
                .await
                .expect("should succeed");
            let explicit = tool
                .execute("test-id", serde_json::json!({ "path": "subdir/" }), None)
                .await
                .expect("should succeed");

            let mut base_lines = get_text_content(&base.content)
                .lines()
                .map(str::to_string)
                .collect::<Vec<_>>();
            let mut explicit_lines = get_text_content(&explicit.content)
                .lines()
                .map(str::to_string)
                .collect::<Vec<_>>();
            base_lines.sort();
            explicit_lines.sort();
            assert_eq!(
                base_lines, explicit_lines,
                "ls results should match for 'subdir' vs 'subdir/'"
            );
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
            let _mode_guard = UnixModeGuard::set(&dir, 0o000);

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
            assert!(
                message.to_lowercase().contains("permission denied"),
                "ls should report PermissionDenied: {message}"
            );
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
    fn test_ls_listing_is_creation_order_invariant() {
        asupersync::test_utils::run_test(|| async {
            async fn listing_for(order: &[(&str, bool)]) -> (String, Option<serde_json::Value>) {
                let temp_dir = tempfile::tempdir().expect("tempdir");
                for (name, is_dir) in order {
                    let path = temp_dir.path().join(name);
                    if *is_dir {
                        std::fs::create_dir_all(&path).expect("create dir");
                    } else {
                        std::fs::write(&path, b"fixture").expect("create file");
                    }
                }

                let tool = pi::tools::LsTool::new(temp_dir.path());
                let result = tool
                    .execute("test-id", serde_json::json!({}), None)
                    .await
                    .expect("ls should succeed");
                (get_text_content(&result.content), result.details)
            }

            let orders = [
                [
                    ("Readme.md", false),
                    ("alpha", true),
                    (".env", false),
                    ("Beta", true),
                ],
                [
                    ("Beta", true),
                    (".env", false),
                    ("Readme.md", false),
                    ("alpha", true),
                ],
                [
                    ("alpha", true),
                    ("Beta", true),
                    ("Readme.md", false),
                    (".env", false),
                ],
            ];

            let mut baseline: Option<String> = None;
            for order in orders {
                let (text, details) = listing_for(&order).await;
                assert!(
                    details.is_none(),
                    "unexpected ls details for stable listing: {details:?}"
                );
                if let Some(expected) = &baseline {
                    assert_eq!(
                        text.as_str(),
                        expected.as_str(),
                        "ls output changed when only creation order changed"
                    );
                } else {
                    baseline = Some(text);
                }
            }
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

#[allow(clippy::cast_precision_loss)]
fn format_size(bytes: usize) -> String {
    const KB: usize = 1024;
    const MB: usize = 1024 * 1024;

    if bytes >= MB {
        format!("{:.1}MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1}KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes}B")
    }
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

const TOOL_DIAGNOSTIC_SCHEMA: &str = "pi.test.tool_diagnostic.v1";
const TOOL_DIAGNOSTIC_MAX_SNAPSHOT_ENTRIES: usize = 256;
const TOOL_ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "HOME",
    "SHELL",
    "USER",
    "LANG",
    "TERM",
    "PWD",
    "TMPDIR",
    "PI_CODING_AGENT_DIR",
    "PI_CONFIG_PATH",
    "PI_SESSIONS_DIR",
    "PI_PACKAGE_DIR",
    "CARGO_TARGET_DIR",
    "RUST_LOG",
];
static TOOL_DIAGNOSTIC_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, serde::Serialize)]
struct WorkspaceEntry {
    path: String,
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    size_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    permissions_octal: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    read_error: Option<String>,
}

#[derive(Debug, serde::Serialize)]
struct WorkspaceSnapshot {
    root: String,
    total_entries: usize,
    truncated: bool,
    entries: Vec<WorkspaceEntry>,
}

#[derive(Debug, serde::Serialize)]
struct ToolTimingBreakdown {
    tool_execute: u64,
    workspace_snapshot: u64,
    diagnostics_capture: u64,
}

#[derive(Debug, serde::Serialize)]
struct ToolExecutionDiagnostic {
    schema: &'static str,
    test: String,
    tool_name: String,
    tool_call_id: String,
    cwd: String,
    workspace_root: String,
    captured_epoch_ms: u128,
    timing_ms: ToolTimingBreakdown,
    allowlisted_env: BTreeMap<String, String>,
    command_transcript: serde_json::Value,
    workspace_snapshot: WorkspaceSnapshot,
}

fn sanitize_artifact_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "unnamed".to_string()
    } else {
        out
    }
}

#[cfg(unix)]
fn permission_octal(metadata: &std::fs::Metadata) -> String {
    format!("{:03o}", metadata.permissions().mode() & 0o777)
}

#[cfg(not(unix))]
fn permission_octal(_metadata: &std::fs::Metadata) -> String {
    "n/a".to_string()
}

fn collect_allowlisted_env() -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    for key in TOOL_ENV_ALLOWLIST {
        if let Ok(value) = std::env::var(key) {
            env.insert((*key).to_string(), value);
        }
    }
    env
}

fn collect_workspace_snapshot(workspace_root: &Path) -> WorkspaceSnapshot {
    let mut entries = Vec::new();
    let mut stack = vec![workspace_root.to_path_buf()];
    let mut truncated = false;

    while let Some(dir) = stack.pop() {
        let read_dir = match std::fs::read_dir(&dir) {
            Ok(read_dir) => read_dir,
            Err(err) => {
                let rel = dir
                    .strip_prefix(workspace_root)
                    .unwrap_or(dir.as_path())
                    .display()
                    .to_string();
                entries.push(WorkspaceEntry {
                    path: if rel.is_empty() { ".".to_string() } else { rel },
                    kind: "unreadable".to_string(),
                    size_bytes: None,
                    permissions_octal: None,
                    read_error: Some(err.to_string()),
                });
                if entries.len() >= TOOL_DIAGNOSTIC_MAX_SNAPSHOT_ENTRIES {
                    truncated = true;
                    break;
                }
                continue;
            }
        };

        let mut dir_entries = read_dir
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect::<Vec<_>>();
        dir_entries.sort();

        for path in dir_entries {
            if entries.len() >= TOOL_DIAGNOSTIC_MAX_SNAPSHOT_ENTRIES {
                truncated = true;
                break;
            }
            let rel = path
                .strip_prefix(workspace_root)
                .unwrap_or(path.as_path())
                .display()
                .to_string();
            let metadata = match std::fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(err) => {
                    entries.push(WorkspaceEntry {
                        path: rel,
                        kind: "unknown".to_string(),
                        size_bytes: None,
                        permissions_octal: None,
                        read_error: Some(err.to_string()),
                    });
                    continue;
                }
            };
            let file_type = metadata.file_type();
            let kind = if file_type.is_dir() {
                "dir"
            } else if file_type.is_file() {
                "file"
            } else if file_type.is_symlink() {
                "symlink"
            } else {
                "other"
            };
            entries.push(WorkspaceEntry {
                path: rel,
                kind: kind.to_string(),
                size_bytes: if file_type.is_file() {
                    Some(metadata.len())
                } else {
                    None
                },
                permissions_octal: {
                    #[cfg(unix)]
                    {
                        Some(permission_octal(&metadata))
                    }
                    #[cfg(not(unix))]
                    {
                        let _ = metadata;
                        None
                    }
                },
                read_error: None,
            });
            if file_type.is_dir() {
                stack.push(path);
            }
        }
        if truncated {
            break;
        }
    }

    WorkspaceSnapshot {
        root: workspace_root.display().to_string(),
        total_entries: entries.len(),
        truncated,
        entries,
    }
}

fn tool_diagnostic_artifact_root() -> PathBuf {
    std::env::var("TEST_TOOL_DIAGNOSTIC_DIR").map_or_else(
        |_| {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("target")
                .join("test-artifacts")
                .join("tool-diagnostics")
        },
        PathBuf::from,
    )
}

fn tool_command_transcript(
    input: &serde_json::Value,
    result: &pi::PiResult<pi::tools::ToolOutput>,
) -> serde_json::Value {
    match result {
        Ok(output) => serde_json::json!({
            "input": input,
            "outcome": "ok",
            "output_text": get_text_content(&output.content),
            "details": output.details.clone(),
            "is_error": output.is_error
        }),
        Err(err) => serde_json::json!({
            "input": input,
            "outcome": "error",
            "error": err.to_string()
        }),
    }
}

fn duration_millis_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn normalize_tool_diagnostic_value(value: &mut serde_json::Value, workspace_root: &str) {
    match value {
        serde_json::Value::String(text) => {
            if !workspace_root.is_empty() && text.contains(workspace_root) {
                *text = text.replace(workspace_root, "<WORKSPACE_ROOT>");
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                normalize_tool_diagnostic_value(item, workspace_root);
            }
        }
        serde_json::Value::Object(map) => {
            for value in map.values_mut() {
                normalize_tool_diagnostic_value(value, workspace_root);
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

fn normalize_tool_diagnostic_for_snapshot(mut diagnostic: serde_json::Value) -> serde_json::Value {
    let workspace_root = diagnostic
        .get("workspace_root")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    normalize_tool_diagnostic_value(&mut diagnostic, &workspace_root);

    if let Some(object) = diagnostic.as_object_mut() {
        object.insert("captured_epoch_ms".to_string(), serde_json::json!(0));
        object.insert(
            "allowlisted_env".to_string(),
            serde_json::json!({"_normalized": "environment-dependent"}),
        );
    }

    if let Some(timing) = diagnostic
        .get_mut("timing_ms")
        .and_then(serde_json::Value::as_object_mut)
    {
        for value in timing.values_mut() {
            *value = serde_json::json!(0);
        }
    }

    if let Some(entries) = diagnostic
        .pointer_mut("/workspace_snapshot/entries")
        .and_then(serde_json::Value::as_array_mut)
    {
        for entry in entries {
            if let Some(entry_object) = entry.as_object_mut()
                && entry_object.contains_key("permissions_octal")
            {
                entry_object.insert(
                    "permissions_octal".to_string(),
                    serde_json::json!("<PERMISSIONS>"),
                );
            }
        }
    }

    // Canonical (sorted) key order, so the golden is stable regardless of
    // whether serde_json's `preserve_order` feature is active. fsqlite's
    // `native` feature currently forces it on transitively (bd-nhm45), which
    // otherwise flips every map to insertion order.
    sort_json_object_keys(&mut diagnostic);
    diagnostic
}

/// Recursively rewrite every JSON object into sorted-key order.
fn sort_json_object_keys(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(object) => {
            let mut entries: Vec<(String, serde_json::Value)> =
                std::mem::take(object).into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            for (_, entry) in &mut entries {
                sort_json_object_keys(entry);
            }
            object.extend(entries);
        }
        serde_json::Value::Array(array) => {
            for entry in array {
                sort_json_object_keys(entry);
            }
        }
        _ => {}
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn normalize_tool_diagnostic_snapshot_is_invariant_to_noise() {
    let baseline = serde_json::json!({
        "schema": TOOL_DIAGNOSTIC_SCHEMA,
        "test": "diagnostic-normalization",
        "tool_name": "read",
        "tool_call_id": "diag-001",
        "cwd": "/tmp/pi-a/project/subdir",
        "workspace_root": "/tmp/pi-a/project",
        "captured_epoch_ms": 1_700_000_000_000u64,
        "timing_ms": {
            "tool_execute": 12,
            "workspace_snapshot": 7,
            "diagnostics_capture": 19
        },
        "allowlisted_env": {
            "HOME": "/home/alice",
            "TMPDIR": "/tmp/pi-a/project/tmp"
        },
        "command_transcript": {
            "input": {
                "path": "/tmp/pi-a/project/data/sample.txt"
            },
            "output": {
                "summary": "read /tmp/pi-a/project/data/sample.txt",
                "nested": [
                    "/tmp/pi-a/project/data/sample.txt",
                    {
                        "config": "/tmp/pi-a/project/.pi/settings.json"
                    }
                ]
            }
        },
        "workspace_snapshot": {
            "entries": [
                {
                    "path": "/tmp/pi-a/project/data/sample.txt",
                    "permissions_octal": "644"
                },
                {
                    "path": "/tmp/pi-a/project/.pi/settings.json",
                    "permissions_octal": "600"
                }
            ]
        }
    });
    let noisy_variant = serde_json::json!({
        "schema": TOOL_DIAGNOSTIC_SCHEMA,
        "test": "diagnostic-normalization",
        "tool_name": "read",
        "tool_call_id": "diag-001",
        "cwd": "/var/tmp/pi-b/project/subdir",
        "workspace_root": "/var/tmp/pi-b/project",
        "captured_epoch_ms": 1_800_000_123_456u64,
        "timing_ms": {
            "tool_execute": 99,
            "workspace_snapshot": 33,
            "diagnostics_capture": 144
        },
        "allowlisted_env": {
            "HOME": "/Users/bob",
            "TMPDIR": "/var/tmp/pi-b/project/tmp"
        },
        "command_transcript": {
            "input": {
                "path": "/var/tmp/pi-b/project/data/sample.txt"
            },
            "output": {
                "summary": "read /var/tmp/pi-b/project/data/sample.txt",
                "nested": [
                    "/var/tmp/pi-b/project/data/sample.txt",
                    {
                        "config": "/var/tmp/pi-b/project/.pi/settings.json"
                    }
                ]
            }
        },
        "workspace_snapshot": {
            "entries": [
                {
                    "path": "/var/tmp/pi-b/project/data/sample.txt",
                    "permissions_octal": "755"
                },
                {
                    "path": "/var/tmp/pi-b/project/.pi/settings.json",
                    "permissions_octal": "700"
                }
            ]
        }
    });

    let normalized_baseline = normalize_tool_diagnostic_for_snapshot(baseline);
    let normalized_variant = normalize_tool_diagnostic_for_snapshot(noisy_variant);

    assert_eq!(normalized_baseline, normalized_variant);
    assert_eq!(
        normalized_baseline["captured_epoch_ms"],
        serde_json::json!(0)
    );
    assert_eq!(
        normalized_baseline["allowlisted_env"],
        serde_json::json!({"_normalized": "environment-dependent"})
    );
    assert_eq!(
        normalized_baseline["workspace_root"],
        serde_json::json!("<WORKSPACE_ROOT>")
    );
    assert_eq!(
        normalized_baseline["workspace_snapshot"]["entries"][0]["permissions_octal"],
        serde_json::json!("<PERMISSIONS>")
    );
}

async fn execute_tool_with_diagnostics<T: Tool + ?Sized>(
    harness: &TestHarness,
    tool: &T,
    tool_name: &str,
    tool_call_id: &str,
    input: serde_json::Value,
) -> pi::PiResult<pi::tools::ToolOutput> {
    let execute_started = Instant::now();
    let result = tool.execute(tool_call_id, input.clone(), None).await;
    log_tool_execution(
        harness,
        tool_name,
        tool_call_id,
        &input,
        execute_started.elapsed(),
        &result,
    );
    result
}

/// Log a tool execution with high-fidelity diagnostics artifact capture.
#[allow(clippy::too_many_lines)]
fn log_tool_execution(
    harness: &TestHarness,
    tool_name: &str,
    tool_call_id: &str,
    input: &serde_json::Value,
    execute_elapsed: Duration,
    result: &pi::PiResult<pi::tools::ToolOutput>,
) {
    let logger = harness.log();
    let workspace_root = harness.temp_dir();
    let snapshot_started = Instant::now();
    let workspace_snapshot = collect_workspace_snapshot(workspace_root);
    let workspace_snapshot_ms = duration_millis_u64(snapshot_started.elapsed());
    let capture_elapsed = execute_elapsed.saturating_add(snapshot_started.elapsed());
    let captured_epoch_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let diagnostic = ToolExecutionDiagnostic {
        schema: TOOL_DIAGNOSTIC_SCHEMA,
        test: harness.name().to_string(),
        tool_name: tool_name.to_string(),
        tool_call_id: tool_call_id.to_string(),
        cwd: workspace_root.display().to_string(),
        workspace_root: workspace_root.display().to_string(),
        captured_epoch_ms,
        timing_ms: ToolTimingBreakdown {
            tool_execute: duration_millis_u64(execute_elapsed),
            workspace_snapshot: workspace_snapshot_ms,
            diagnostics_capture: duration_millis_u64(capture_elapsed),
        },
        allowlisted_env: collect_allowlisted_env(),
        command_transcript: tool_command_transcript(input, result),
        workspace_snapshot,
    };

    let test_component = sanitize_artifact_component(harness.name());
    let sequence = TOOL_DIAGNOSTIC_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let artifact_name = format!(
        "{sequence:05}-{}-{}.json",
        sanitize_artifact_component(tool_name),
        sanitize_artifact_component(tool_call_id)
    );
    let artifact_path = tool_diagnostic_artifact_root()
        .join(test_component)
        .join(artifact_name);

    let artifact_write_result = (|| -> std::io::Result<()> {
        if let Some(parent) = artifact_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(&diagnostic)
            .map_err(|err| std::io::Error::other(err.to_string()))?;
        std::fs::write(&artifact_path, bytes)
    })();

    match artifact_write_result {
        Ok(()) => {
            harness.record_artifact(format!("tool-diagnostic:{tool_call_id}"), &artifact_path);
            logger.info_ctx("tool_diag", "wrote tool diagnostics artifact", |ctx| {
                ctx.push(("tool_name".into(), tool_name.to_string()));
                ctx.push(("tool_call_id".into(), tool_call_id.to_string()));
                ctx.push(("artifact_path".into(), artifact_path.display().to_string()));
                ctx.push((
                    "execute_ms".into(),
                    duration_millis_u64(execute_elapsed).to_string(),
                ));
                ctx.push((
                    "workspace_snapshot_ms".into(),
                    workspace_snapshot_ms.to_string(),
                ));
            });
        }
        Err(err) => {
            logger.with_context(
                common::logging::LogLevel::Warn,
                "tool_diag",
                "failed to write diagnostics artifact",
                |ctx| {
                    ctx.push(("tool_name".into(), tool_name.to_string()));
                    ctx.push(("tool_call_id".into(), tool_call_id.to_string()));
                    ctx.push(("artifact_path".into(), artifact_path.display().to_string()));
                    ctx.push(("error".into(), err.to_string()));
                },
            );
        }
    }

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
                ctx.push((
                    "execute_ms".into(),
                    duration_millis_u64(execute_elapsed).to_string(),
                ));
            });
        }
        Err(e) => {
            let err_str = e.to_string();
            logger.info_ctx("tool_exec", format!("{tool_name} errored"), |ctx| {
                ctx.push(("tool_call_id".into(), tool_call_id.to_string()));
                ctx.push(("input".into(), input.to_string()));
                ctx.push(("error".into(), err_str.clone()));
                ctx.push((
                    "execute_ms".into(),
                    duration_millis_u64(execute_elapsed).to_string(),
                ));
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

            let result =
                execute_tool_with_diagnostics(&harness, &tool, "read", "read-001", input.clone())
                    .await;

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

            let result =
                execute_tool_with_diagnostics(&harness, &tool, "read", "read-002", input.clone())
                    .await;

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

            let result =
                execute_tool_with_diagnostics(&harness, &tool, "read", "read-003", input.clone())
                    .await;

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

            let result =
                execute_tool_with_diagnostics(&harness, &tool, "read", "read-004", input.clone())
                    .await;

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

    #[test]
    fn e2e_read_diagnostic_artifact_matches_golden_contract() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("e2e_read_diagnostic_artifact_matches_golden_contract");
            let path = harness.create_file("sample.txt", b"alpha\nbeta\ngamma");
            let tool = pi::tools::ReadTool::new(harness.temp_dir());
            let input = serde_json::json!({
                "path": path.to_string_lossy()
            });

            let result =
                execute_tool_with_diagnostics(&harness, &tool, "read", "read-golden-001", input)
                    .await;

            let output = result.expect("read golden probe should succeed");
            let text = get_text_content(&output.content);
            assert!(text.contains("alpha"));
            assert!(text.contains("gamma"));

            let artifacts = harness.log().artifacts();
            let diagnostic_artifact = artifacts
                .iter()
                .find(|entry| entry.name == "tool-diagnostic:read-golden-001")
                .expect("expected diagnostics artifact entry");
            let diagnostic_body = std::fs::read_to_string(&diagnostic_artifact.path)
                .expect("expected diagnostics artifact file to be readable");
            let diagnostic_json: serde_json::Value =
                serde_json::from_str(&diagnostic_body).expect("expected valid diagnostics JSON");
            let normalized = normalize_tool_diagnostic_for_snapshot(diagnostic_json);
            let pretty = serde_json::to_string_pretty(&normalized)
                .expect("normalized diagnostic should serialize");

            let normalized_path = harness.temp_path("tool-diagnostic-read.normalized.json");
            std::fs::write(&normalized_path, &pretty)
                .expect("write normalized diagnostic artifact");
            harness.record_artifact("tool-diagnostic:read-golden-normalized", &normalized_path);

            insta::assert_snapshot!("tool_diagnostic_read_success", pretty);
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

            let result =
                execute_tool_with_diagnostics(&harness, &tool, "write", "write-001", input.clone())
                    .await;

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

            let result =
                execute_tool_with_diagnostics(&harness, &tool, "write", "write-002", input.clone())
                    .await;

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

            let result =
                execute_tool_with_diagnostics(&harness, &tool, "edit", "edit-001", input.clone())
                    .await;

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

            let result =
                execute_tool_with_diagnostics(&harness, &tool, "edit", "edit-002", input.clone())
                    .await;

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

            let result =
                execute_tool_with_diagnostics(&harness, &tool, "bash", "bash-001", input.clone())
                    .await;

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

            let result =
                execute_tool_with_diagnostics(&harness, &tool, "bash", "bash-002", input.clone())
                    .await;

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

            let result =
                execute_tool_with_diagnostics(&harness, &tool, "bash", "bash-003", input.clone())
                    .await;

            // Non-zero command exits are successful tool invocations with is_error=true.
            let output = result.expect("nonexistent command should return a tool error output");
            assert!(output.is_error, "nonexistent command must set is_error");
            let message = get_text_content(&output.content);
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

            let result =
                execute_tool_with_diagnostics(&harness, &tool, "bash", "bash-004", input.clone())
                    .await;

            let output = result.expect("timeout should return a tool error output");
            assert!(output.is_error, "timeout must set is_error");
            let message = get_text_content(&output.content);
            assert!(message.contains("timed out"));
        });
    }

    #[test]
    fn e2e_bash_diagnostic_artifact_contains_required_fields() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("e2e_bash_diagnostic_artifact_contains_required_fields");
            let tool = pi::tools::BashTool::new(harness.temp_dir());
            let input = serde_json::json!({
                "command": "echo diagnostic_probe"
            });

            let result = execute_tool_with_diagnostics(
                &harness,
                &tool,
                "bash",
                "bash-diag-001",
                input.clone(),
            )
            .await;
            let output = result.expect("diagnostic probe should succeed");
            let text = get_text_content(&output.content);
            assert!(text.contains("diagnostic_probe"));

            let artifacts = harness.log().artifacts();
            let diagnostic_artifact = artifacts
                .iter()
                .find(|entry| entry.name == "tool-diagnostic:bash-diag-001")
                .expect("expected diagnostics artifact entry");
            let diagnostic_body = std::fs::read_to_string(&diagnostic_artifact.path)
                .expect("expected diagnostics artifact file to be readable");
            let diagnostic_json: serde_json::Value =
                serde_json::from_str(&diagnostic_body).expect("expected valid diagnostics JSON");

            assert_eq!(
                diagnostic_json.get("schema"),
                Some(&serde_json::Value::String(
                    TOOL_DIAGNOSTIC_SCHEMA.to_string()
                ))
            );
            assert_eq!(
                diagnostic_json.get("tool_call_id"),
                Some(&serde_json::Value::String("bash-diag-001".to_string()))
            );
            assert!(diagnostic_json.get("command_transcript").is_some());
            assert!(diagnostic_json.get("workspace_snapshot").is_some());
            assert!(diagnostic_json.get("allowlisted_env").is_some());
            assert!(diagnostic_json.get("timing_ms").is_some());
            assert!(
                diagnostic_json
                    .pointer("/timing_ms/tool_execute")
                    .and_then(serde_json::Value::as_u64)
                    .is_some(),
                "expected timing breakdown with tool_execute"
            );
            assert!(
                diagnostic_json
                    .pointer("/workspace_snapshot/entries")
                    .and_then(serde_json::Value::as_array)
                    .is_some(),
                "expected workspace snapshot entries"
            );
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

            let result =
                execute_tool_with_diagnostics(&harness, &tool, "grep", "grep-001", input.clone())
                    .await;

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

            let result =
                execute_tool_with_diagnostics(&harness, &tool, "grep", "grep-002", input.clone())
                    .await;

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

            let result =
                execute_tool_with_diagnostics(&harness, &tool, "grep", "grep-003", input.clone())
                    .await;

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

            let result =
                execute_tool_with_diagnostics(&harness, &tool, "find", "find-001", input.clone())
                    .await;

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

            let result =
                execute_tool_with_diagnostics(&harness, &tool, "find", "find-002", input.clone())
                    .await;

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

            let result =
                execute_tool_with_diagnostics(&harness, &tool, "ls", "ls-001", input.clone()).await;

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

            let result =
                execute_tool_with_diagnostics(&harness, &tool, "ls", "ls-002", input.clone()).await;

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

            let result =
                execute_tool_with_diagnostics(&harness, &tool, "ls", "ls-003", input.clone()).await;

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

            let result =
                execute_tool_with_diagnostics(&harness, &tool, "ls", "ls-004", input.clone()).await;

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

/// Comprehensive E2E: exercise the core filesystem/process tools in a single test workspace with full artifact logging.
#[test]
#[allow(clippy::too_many_lines)]
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
        let result = execute_tool_with_diagnostics(
            &harness,
            &write_tool,
            "write",
            "rt-write",
            write_input.clone(),
        )
        .await;
        result.expect("write should succeed");

        // Read the file back
        harness.section("Read");
        let read_tool = pi::tools::ReadTool::new(harness.temp_dir());
        let read_input = serde_json::json!({
            "path": harness.temp_path("project/hello.rs").to_string_lossy().to_string()
        });
        let result = execute_tool_with_diagnostics(
            &harness,
            &read_tool,
            "read",
            "rt-read",
            read_input.clone(),
        )
        .await;
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
        let result = execute_tool_with_diagnostics(
            &harness,
            &edit_tool,
            "edit",
            "rt-edit",
            edit_input.clone(),
        )
        .await;
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
        let result =
            execute_tool_with_diagnostics(&harness, &ls_tool, "ls", "rt-ls", ls_input.clone())
                .await;
        let output = result.expect("ls should succeed");
        let text = get_text_content(&output.content);
        assert!(text.contains("hello.rs"));

        // Bash
        harness.section("Bash");
        let bash_tool = pi::tools::BashTool::new(harness.temp_dir());
        let bash_input = serde_json::json!({
            "command": "wc -l project/hello.rs"
        });
        let result = execute_tool_with_diagnostics(
            &harness,
            &bash_tool,
            "bash",
            "rt-bash",
            bash_input.clone(),
        )
        .await;
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
            let result = execute_tool_with_diagnostics(
                &harness,
                &grep_tool,
                "grep",
                "rt-grep",
                grep_input.clone(),
            )
            .await;
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
            let result = execute_tool_with_diagnostics(
                &harness,
                &find_tool,
                "find",
                "rt-find",
                find_input.clone(),
            )
            .await;
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

// ============================================================================
// Security abuse-case regression tests (bd-1f42.5.1)
// ============================================================================
// These tests document the security boundary of the tool layer.
// Tools intentionally rely on agent-level trust (the LLM) rather than
// tool-level sandboxing.  These tests verify current behaviour so that
// any future tightening is deliberate, not accidental.

mod security_path_traversal {
    use super::*;

    /// Read tool rejects parent-directory traversal (`../`).
    #[test]
    fn read_parent_dir_traversal() {
        asupersync::test_utils::run_test(|| async {
            let parent = tempfile::tempdir().unwrap();
            let child_dir = parent.path().join("child");
            std::fs::create_dir_all(&child_dir).unwrap();
            let outside_file = parent.path().join("outside.txt");
            std::fs::write(&outside_file, "OUTSIDE_DATA").unwrap();

            let tool = pi::tools::ReadTool::new(&child_dir);
            let input = serde_json::json!({
                "path": "../outside.txt"
            });
            let err = tool
                .execute("sec-read-01", input, None)
                .await
                .expect_err("read with ../ should be rejected");
            let text = err.to_string();
            assert!(
                text.contains("Cannot read outside the working directory"),
                "expected outside-cwd rejection: {text}"
            );
        });
    }

    /// Read tool rejects dot-dot escape even when path variants match on disk.
    #[test]
    fn read_path_variant_outside_cwd_is_rejected() {
        asupersync::test_utils::run_test(|| async {
            let parent = tempfile::tempdir().unwrap();
            let child_dir = parent.path().join("child");
            std::fs::create_dir_all(child_dir.join("subdir")).unwrap();
            let outside_file = parent.path().join("outside.txt");
            std::fs::write(&outside_file, "OUTSIDE").unwrap();

            let tool = pi::tools::ReadTool::new(&child_dir);
            let input = serde_json::json!({
                "path": "subdir/../../outside.txt"
            });
            let err = tool
                .execute("sec-read-04", input, None)
                .await
                .expect_err("dot-dot escape should be rejected");
            let text = err.to_string();
            assert!(
                text.contains("Cannot read outside the working directory"),
                "expected outside-cwd rejection: {text}"
            );
        });
    }

    /// Write tool rejects parent-directory traversal (`../`).
    #[test]
    fn write_parent_dir_traversal() {
        asupersync::test_utils::run_test(|| async {
            let parent = tempfile::tempdir().unwrap();
            let child_dir = parent.path().join("child");
            std::fs::create_dir_all(&child_dir).unwrap();

            let tool = pi::tools::WriteTool::new(&child_dir);
            let escaped_path = child_dir.join("../escaped.txt");
            let input = serde_json::json!({
                "path": escaped_path.to_string_lossy(),
                "content": "ESCAPED_CONTENT"
            });
            let err = tool
                .execute("sec-write-01", input, None)
                .await
                .expect_err("write with ../ should be rejected");
            let text = err.to_string();
            assert!(
                text.contains("Cannot write outside the working directory"),
                "expected outside-cwd rejection: {text}"
            );
            assert!(
                !parent.path().join("escaped.txt").exists(),
                "write should not escape cwd"
            );
        });
    }

    /// Write tool rejects dot-dot escape paths that resolve outside CWD.
    #[test]
    fn write_path_variant_outside_cwd_is_rejected() {
        asupersync::test_utils::run_test(|| async {
            let parent = tempfile::tempdir().unwrap();
            let child_dir = parent.path().join("child");
            std::fs::create_dir_all(child_dir.join("subdir")).unwrap();

            let tool = pi::tools::WriteTool::new(&child_dir);
            let escaped_path = child_dir.join("subdir/../../escaped.txt");
            let input = serde_json::json!({
                "path": escaped_path.to_string_lossy(),
                "content": "ESCAPED_CONTENT"
            });
            let err = tool
                .execute("sec-write-03", input, None)
                .await
                .expect_err("dot-dot escape should be rejected");
            let text = err.to_string();
            assert!(
                text.contains("Cannot write outside the working directory"),
                "expected outside-cwd rejection: {text}"
            );
            assert!(
                !parent.path().join("escaped.txt").exists(),
                "write should not escape cwd"
            );
        });
    }

    /// Edit tool rejects parent-directory traversal (`../`).
    #[test]
    fn edit_parent_dir_traversal() {
        asupersync::test_utils::run_test(|| async {
            let parent = tempfile::tempdir().unwrap();
            let child_dir = parent.path().join("child");
            std::fs::create_dir_all(&child_dir).unwrap();
            let target = parent.path().join("target.txt");
            std::fs::write(&target, "ORIGINAL_CONTENT").unwrap();

            let tool = pi::tools::EditTool::new(&child_dir);
            let escaped_path = child_dir.join("../target.txt");
            let input = serde_json::json!({
                "path": escaped_path.to_string_lossy(),
                "oldText": "ORIGINAL_CONTENT",
                "newText": "MODIFIED_CONTENT"
            });
            let err = tool
                .execute("sec-edit-01", input, None)
                .await
                .expect_err("edit with ../ should be rejected");
            let text = err.to_string();
            assert!(
                text.contains("Cannot edit outside the working directory"),
                "expected outside-cwd rejection: {text}"
            );
            let content = std::fs::read_to_string(&target).unwrap();
            assert_eq!(content, "ORIGINAL_CONTENT");
        });
    }

    /// Edit tool rejects dot-dot escape paths that resolve outside CWD.
    #[test]
    fn edit_path_variant_outside_cwd_is_rejected() {
        asupersync::test_utils::run_test(|| async {
            let parent = tempfile::tempdir().unwrap();
            let child_dir = parent.path().join("child");
            std::fs::create_dir_all(child_dir.join("subdir")).unwrap();
            let target = parent.path().join("target.txt");
            std::fs::write(&target, "ORIGINAL_CONTENT").unwrap();

            let tool = pi::tools::EditTool::new(&child_dir);
            let escaped_path = child_dir.join("subdir/../../target.txt");
            let input = serde_json::json!({
                "path": escaped_path.to_string_lossy(),
                "oldText": "ORIGINAL_CONTENT",
                "newText": "MODIFIED_CONTENT"
            });
            let err = tool
                .execute("sec-edit-02", input, None)
                .await
                .expect_err("dot-dot escape should be rejected");
            let text = err.to_string();
            assert!(
                text.contains("Cannot edit outside the working directory"),
                "expected outside-cwd rejection: {text}"
            );
            let content = std::fs::read_to_string(&target).unwrap();
            assert_eq!(content, "ORIGINAL_CONTENT");
        });
    }

    /// Read tool rejects absolute paths outside CWD.
    #[test]
    fn read_absolute_path_outside_cwd() {
        asupersync::test_utils::run_test(|| async {
            let outside = tempfile::tempdir().unwrap();
            let outside_file = outside.path().join("outside.txt");
            std::fs::write(&outside_file, "OUTSIDE_DATA").unwrap();

            let cwd = tempfile::tempdir().unwrap();
            let tool = pi::tools::ReadTool::new(cwd.path());
            let input = serde_json::json!({
                "path": outside_file.to_string_lossy()
            });
            let err = tool
                .execute("sec-read-02", input, None)
                .await
                .expect_err("absolute path outside CWD should be rejected");
            let text = err.to_string();
            assert!(text.contains("Cannot read outside the working directory"));
        });
    }

    /// Write tool rejects absolute paths outside CWD.
    #[test]
    fn write_absolute_path_outside_cwd() {
        asupersync::test_utils::run_test(|| async {
            let outside = tempfile::tempdir().unwrap();
            let outside_file = outside.path().join("outside.txt");

            let cwd = tempfile::tempdir().unwrap();
            let tool = pi::tools::WriteTool::new(cwd.path());
            let input = serde_json::json!({
                "path": outside_file.to_string_lossy(),
                "content": "NOPE"
            });
            let err = tool
                .execute("sec-write-04", input, None)
                .await
                .expect_err("absolute path outside CWD should be rejected");
            let text = err.to_string();
            assert!(
                text.contains("Cannot write outside the working directory"),
                "expected outside-cwd rejection: {text}"
            );
            assert!(
                !outside_file.exists(),
                "write should not touch outside path"
            );
        });
    }

    /// Edit tool rejects absolute paths outside CWD.
    #[test]
    fn edit_absolute_path_outside_cwd() {
        asupersync::test_utils::run_test(|| async {
            let outside = tempfile::tempdir().unwrap();
            let outside_file = outside.path().join("outside.txt");
            std::fs::write(&outside_file, "ORIGINAL").unwrap();

            let cwd = tempfile::tempdir().unwrap();
            let tool = pi::tools::EditTool::new(cwd.path());
            let input = serde_json::json!({
                "path": outside_file.to_string_lossy(),
                "oldText": "ORIGINAL",
                "newText": "MODIFIED"
            });
            let err = tool
                .execute("sec-edit-03", input, None)
                .await
                .expect_err("absolute path outside CWD should be rejected");
            let text = err.to_string();
            assert!(
                text.contains("Cannot edit outside the working directory"),
                "expected outside-cwd rejection: {text}"
            );
            let content = std::fs::read_to_string(&outside_file).unwrap();
            assert_eq!(content, "ORIGINAL");
        });
    }

    /// Read tool rejects symlinks that resolve outside CWD.
    #[test]
    #[cfg(unix)]
    fn read_symlink_escape() {
        asupersync::test_utils::run_test(|| async {
            let outside = tempfile::tempdir().unwrap();
            let outside_file = outside.path().join("outside.txt");
            std::fs::write(&outside_file, "SYMLINK_OUTSIDE").unwrap();

            let cwd = tempfile::tempdir().unwrap();
            let link = cwd.path().join("link.txt");
            std::os::unix::fs::symlink(&outside_file, &link).unwrap();

            let tool = pi::tools::ReadTool::new(cwd.path());
            let input = serde_json::json!({
                "path": link.to_string_lossy()
            });
            let err = tool
                .execute("sec-read-03", input, None)
                .await
                .expect_err("symlink escape should be rejected");
            let text = err.to_string();
            assert!(
                text.contains("Cannot read outside the working directory"),
                "expected outside-cwd rejection: {text}"
            );
        });
    }

    /// Write tool rejects symlinks that resolve outside CWD.
    #[test]
    #[cfg(unix)]
    fn write_symlink_escape_is_rejected() {
        asupersync::test_utils::run_test(|| async {
            let outside = tempfile::tempdir().unwrap();
            let target = outside.path().join("target.txt");
            std::fs::write(&target, "ORIGINAL").unwrap();

            let cwd = tempfile::tempdir().unwrap();
            let link = cwd.path().join("link.txt");
            std::os::unix::fs::symlink(&target, &link).unwrap();

            let tool = pi::tools::WriteTool::new(cwd.path());
            let input = serde_json::json!({
                "path": link.to_string_lossy(),
                "content": "NEW_CONTENT"
            });
            let err = tool
                .execute("sec-write-02", input, None)
                .await
                .expect_err("write at symlink path should be rejected");
            let text = err.to_string();
            assert!(
                text.contains("Cannot write outside the working directory"),
                "expected outside-cwd rejection: {text}"
            );
            assert!(
                link.symlink_metadata().unwrap().file_type().is_symlink(),
                "symlink should remain intact"
            );
            let target_content = std::fs::read_to_string(&target).unwrap();
            assert_eq!(target_content, "ORIGINAL");
        });
    }

    /// Write tool allows symlinks that resolve inside CWD.
    #[test]
    #[cfg(unix)]
    fn write_symlink_within_cwd_allowed() {
        asupersync::test_utils::run_test(|| async {
            let cwd = tempfile::tempdir().unwrap();
            let target = cwd.path().join("target.txt");
            std::fs::write(&target, "ORIGINAL").unwrap();
            let link = cwd.path().join("link.txt");
            std::os::unix::fs::symlink(&target, &link).unwrap();

            let tool = pi::tools::WriteTool::new(cwd.path());
            let input = serde_json::json!({
                "path": link.to_string_lossy(),
                "content": "NEW_CONTENT"
            });
            let result = tool.execute("sec-write-05", input, None).await;
            result.expect("write at symlink inside cwd should succeed");

            let target_content = std::fs::read_to_string(&target).unwrap();
            assert_eq!(target_content, "NEW_CONTENT");
        });
    }

    /// Read tool allows symlinks that resolve inside CWD.
    #[test]
    #[cfg(unix)]
    fn read_symlink_within_cwd_allowed() {
        asupersync::test_utils::run_test(|| async {
            let cwd = tempfile::tempdir().unwrap();
            let target = cwd.path().join("target.txt");
            std::fs::write(&target, "INSIDE_SECRET").unwrap();
            let link = cwd.path().join("link.txt");
            std::os::unix::fs::symlink(&target, &link).unwrap();

            let tool = pi::tools::ReadTool::new(cwd.path());
            let input = serde_json::json!({
                "path": link.to_string_lossy()
            });
            let result = tool.execute("sec-read-05", input, None).await;
            let output = result.expect("read at symlink inside cwd should succeed");
            let text = get_text_content(&output.content);
            assert!(text.contains("INSIDE_SECRET"));
        });
    }
}

mod security_command_injection {
    use super::*;

    /// Bash tool's stdin is null – commands cannot read piped input.
    #[test]
    fn bash_stdin_is_null() {
        asupersync::test_utils::run_test(|| async {
            let cwd = tempfile::tempdir().unwrap();
            let tool = pi::tools::BashTool::new(cwd.path());
            let input = serde_json::json!({
                "command": "read -t 1 line; echo \"got: $line\""
            });
            let result = tool.execute("sec-bash-01", input, None).await;
            // read from null stdin should fail or produce empty
            let output = result.expect("bash should succeed even with null stdin");
            let text = get_text_content(&output.content);
            assert!(
                text.contains("got: ") || text.contains("got:"),
                "stdin should be empty/null: {text}"
            );
            // The value after "got: " should be empty
            assert!(
                !text.contains("got: malicious"),
                "stdin should not contain injected data"
            );
        });
    }

    /// Bash tool installs EXIT trap for cleanup.
    #[test]
    fn bash_has_exit_trap() {
        asupersync::test_utils::run_test(|| async {
            let cwd = tempfile::tempdir().unwrap();
            let tool = pi::tools::BashTool::new(cwd.path());
            let input = serde_json::json!({
                "command": "trap -p EXIT"
            });
            let result = tool.execute("sec-bash-02", input, None).await;
            let output = result.expect("trap -p should succeed");
            let text = get_text_content(&output.content);
            // Tool installs an EXIT trap
            assert!(
                text.contains("EXIT") || text.contains("exit"),
                "expected EXIT trap to be set: {text}"
            );
        });
    }

    /// Bash tool executes shell metacharacters (;, &&, ||, pipes) – by design.
    #[test]
    fn bash_metacharacter_execution() {
        asupersync::test_utils::run_test(|| async {
            let cwd = tempfile::tempdir().unwrap();
            let tool = pi::tools::BashTool::new(cwd.path());
            let input = serde_json::json!({
                "command": "echo A; echo B && echo C || echo D | cat"
            });
            let result = tool.execute("sec-bash-03", input, None).await;
            let output = result.expect("metacharacters should execute");
            let text = get_text_content(&output.content);
            assert!(text.contains('A'), "semicolon chaining should work: {text}");
            assert!(text.contains('B'), "echo B should execute: {text}");
            assert!(text.contains('C'), "conditional && should work: {text}");
        });
    }

    /// Bash tool executes command substitution – by design.
    #[test]
    fn bash_command_substitution() {
        asupersync::test_utils::run_test(|| async {
            let cwd = tempfile::tempdir().unwrap();
            let tool = pi::tools::BashTool::new(cwd.path());
            let input = serde_json::json!({
                "command": "echo \"user: $(whoami)\""
            });
            let result = tool.execute("sec-bash-04", input, None).await;
            let output = result.expect("command substitution should work");
            let text = get_text_content(&output.content);
            assert!(
                text.contains("user: "),
                "command substitution should execute: {text}"
            );
            // Should contain a real username, not the literal $(whoami)
            assert!(
                !text.contains("$(whoami)"),
                "substitution should be expanded, not literal: {text}"
            );
        });
    }
}

mod security_environment {
    use super::*;

    /// Bash tool inherits the parent process environment.
    #[test]
    fn bash_env_inheritance() {
        asupersync::test_utils::run_test(|| async {
            let cwd = tempfile::tempdir().unwrap();
            let tool = pi::tools::BashTool::new(cwd.path());
            // PATH must be inherited for any command to work
            let input = serde_json::json!({
                "command": "echo \"PATH=$PATH\""
            });
            let result = tool.execute("sec-env-01", input, None).await;
            let output = result.expect("env should be accessible");
            let text = get_text_content(&output.content);
            assert!(
                text.contains("PATH=/"),
                "PATH should be inherited from parent: {text}"
            );
        });
    }

    /// Bash tool CWD matches the configured working directory.
    #[test]
    fn bash_cwd_matches_configured() {
        asupersync::test_utils::run_test(|| async {
            let cwd = tempfile::tempdir().unwrap();
            let tool = pi::tools::BashTool::new(cwd.path());
            let input = serde_json::json!({
                "command": "pwd"
            });
            let result = tool.execute("sec-env-02", input, None).await;
            let output = result.expect("pwd should succeed");
            let text = get_text_content(&output.content);
            // Canonicalize both for comparison (temp dirs may have symlinks)
            let expected = std::fs::canonicalize(cwd.path())
                .unwrap()
                .to_string_lossy()
                .to_string();
            let actual = text.trim().to_string();
            assert!(
                actual.contains(&expected) || expected.contains(&actual),
                "CWD should match configured dir: actual={actual}, expected={expected}"
            );
        });
    }

    /// Bash tool can access HOME environment variable – by design.
    #[test]
    fn bash_home_accessible() {
        asupersync::test_utils::run_test(|| async {
            let cwd = tempfile::tempdir().unwrap();
            let tool = pi::tools::BashTool::new(cwd.path());
            let input = serde_json::json!({
                "command": "echo $HOME"
            });
            let result = tool.execute("sec-env-03", input, None).await;
            let output = result.expect("HOME should be accessible");
            let text = get_text_content(&output.content);
            assert!(!text.trim().is_empty(), "HOME should be non-empty: {text}");
        });
    }

    /// Bash tool inherits the current locale variables when set.
    #[test]
    fn bash_locale_inheritance() {
        asupersync::test_utils::run_test(|| async {
            let cwd = tempfile::tempdir().unwrap();
            let candidate_keys = ["LC_ALL", "LC_CTYPE", "LANG"];
            let mut selected = None;
            for key in candidate_keys {
                if let Ok(value) = std::env::var(key)
                    && !value.trim().is_empty()
                {
                    selected = Some((key, value));
                    break;
                }
            }

            let Some((key, expected)) = selected else {
                // If no locale vars are set in the environment, skip this test rather than
                // mutating the process environment (set_var is unsafe in Rust 2024).
                return;
            };

            let tool = pi::tools::BashTool::new(cwd.path());
            let input = serde_json::json!({
                "command": format!("echo ${key}")
            });
            let result = tool.execute("sec-env-04", input, None).await;
            let output = result.expect("LC_ALL should be accessible");
            let text = get_text_content(&output.content);
            assert!(
                text.to_lowercase().contains(&expected.to_lowercase()),
                "{key} should be inherited: actual={text}, expected={expected}"
            );
        });
    }
}

mod security_unsafe_writes {
    use super::*;

    /// Write tool creates deeply nested directories automatically.
    #[test]
    fn write_creates_arbitrary_dirs() {
        asupersync::test_utils::run_test(|| async {
            let cwd = tempfile::tempdir().unwrap();
            let tool = pi::tools::WriteTool::new(cwd.path());
            let deep_path = cwd.path().join("a/b/c/d/e/f/deeply_nested.txt");
            let input = serde_json::json!({
                "path": deep_path.to_string_lossy(),
                "content": "DEEP_CONTENT"
            });
            let result = tool.execute("sec-write-03", input, None).await;
            result.expect("write should auto-create dirs");
            assert!(deep_path.exists());
            let content = std::fs::read_to_string(&deep_path).unwrap();
            assert_eq!(content, "DEEP_CONTENT");
        });
    }

    /// Write tool overwrites files without backup – by design.
    #[test]
    fn write_overwrites_without_backup() {
        asupersync::test_utils::run_test(|| async {
            let cwd = tempfile::tempdir().unwrap();
            let file = cwd.path().join("overwrite_me.txt");
            std::fs::write(&file, "ORIGINAL_VALUABLE_DATA").unwrap();

            let tool = pi::tools::WriteTool::new(cwd.path());
            let input = serde_json::json!({
                "path": file.to_string_lossy(),
                "content": "REPLACEMENT"
            });
            let result = tool.execute("sec-write-04", input, None).await;
            result.expect("overwrite should succeed");

            let content = std::fs::read_to_string(&file).unwrap();
            assert_eq!(content, "REPLACEMENT");

            // No backup file should exist
            let backup = cwd.path().join("overwrite_me.txt.bak");
            assert!(!backup.exists(), "no backup is created (by design)");
        });
    }

    /// Write tool does not create temp files – writes directly.
    #[test]
    fn write_no_temp_files() {
        asupersync::test_utils::run_test(|| async {
            let cwd = tempfile::tempdir().unwrap();
            let file = cwd.path().join("direct_write.txt");

            let tool = pi::tools::WriteTool::new(cwd.path());
            let input = serde_json::json!({
                "path": file.to_string_lossy(),
                "content": "DIRECT"
            });
            let result = tool.execute("sec-write-05", input, None).await;
            result.expect("write should succeed");

            // Only the target file should exist in the directory
            let entries: Vec<_> = std::fs::read_dir(cwd.path()).unwrap().flatten().collect();
            assert_eq!(
                entries.len(),
                1,
                "only the target file should exist, found: {:?}",
                entries
                    .iter()
                    .map(std::fs::DirEntry::file_name)
                    .collect::<Vec<_>>()
            );
        });
    }

    /// Write tool can create files with potentially dangerous names.
    #[test]
    fn write_dangerous_filenames() {
        asupersync::test_utils::run_test(|| async {
            let cwd = tempfile::tempdir().unwrap();
            let tool = pi::tools::WriteTool::new(cwd.path());

            // File starting with dot (hidden)
            let hidden = cwd.path().join(".hidden_config");
            let input = serde_json::json!({
                "path": hidden.to_string_lossy(),
                "content": "hidden"
            });
            let result = tool.execute("sec-write-06a", input, None).await;
            result.expect("hidden file creation should succeed");
            assert!(hidden.exists());

            // File with spaces
            let spaced = cwd.path().join("file with spaces.txt");
            let input = serde_json::json!({
                "path": spaced.to_string_lossy(),
                "content": "spaced"
            });
            let result = tool.execute("sec-write-06b", input, None).await;
            result.expect("spaced filename should succeed");
            assert!(spaced.exists());
        });
    }

    /// Edit tool operates directly on files, no copy-on-write.
    #[test]
    fn edit_no_copy_on_write() {
        asupersync::test_utils::run_test(|| async {
            let cwd = tempfile::tempdir().unwrap();
            let file = cwd.path().join("edit_target.txt");
            std::fs::write(&file, "BEFORE_EDIT").unwrap();

            let tool = pi::tools::EditTool::new(cwd.path());
            let input = serde_json::json!({
                "path": file.to_string_lossy(),
                "oldText": "BEFORE_EDIT",
                "newText": "AFTER_EDIT"
            });
            let result = tool.execute("sec-edit-02", input, None).await;
            result.expect("edit should succeed");

            let content = std::fs::read_to_string(&file).unwrap();
            assert_eq!(content, "AFTER_EDIT");

            // No temp/backup files should exist
            let entries: Vec<_> = std::fs::read_dir(cwd.path()).unwrap().flatten().collect();
            assert_eq!(
                entries.len(),
                1,
                "only the target file should exist after edit"
            );
        });
    }
}

mod hashline_edit_tool {
    use super::*;

    /// Get the hashline tag for a specific line by reading with hashline=true
    async fn get_hashline_tag(
        tool: &pi::tools::ReadTool,
        path: &std::path::Path,
        line_num: usize,
    ) -> String {
        let input = serde_json::json!({
            "path": path.to_string_lossy(),
            "hashline": true
        });
        let result = tool
            .execute("test-id", input, None)
            .await
            .expect("read with hashline should succeed");
        let text = get_text_content(&result.content);

        let line_prefix = format!("{line_num}#");
        text.lines()
            .find(|line| line.starts_with(&line_prefix))
            .and_then(|line| line.split_once(':'))
            .map(|(tag, _)| tag.to_string())
            .expect("expected hashline tag for requested line")
    }

    #[test]
    fn test_basic_replace() {
        asupersync::test_utils::run_test(|| async {
            let temp_dir = tempfile::tempdir().unwrap();
            let test_file = temp_dir.path().join("test.txt");
            std::fs::write(&test_file, "line1\nOLD_LINE\nline3").unwrap();

            // Get the hashline tag for line 2
            let read_tool = pi::tools::ReadTool::new(temp_dir.path());
            let line2_tag = get_hashline_tag(&read_tool, &test_file, 2).await;

            let tool = pi::tools::HashlineEditTool::new(temp_dir.path());
            let input = serde_json::json!({
                "path": test_file.to_string_lossy(),
                "edits": [{
                    "op": "replace",
                    "pos": line2_tag,
                    "lines": "NEW_LINE"
                }]
            });

            let result = tool
                .execute("test-id", input, None)
                .await
                .expect("edit should succeed");

            // Verify the edit was applied
            let content = std::fs::read_to_string(&test_file).unwrap();
            assert_eq!(content, "line1\nNEW_LINE\nline3");

            // Just verify the tool executed successfully - content may be empty
            assert!(result.details.is_none() || result.details.as_ref().unwrap().is_object());
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_permission_denied_owner_class_does_not_borrow_other_bits() {
        asupersync::test_utils::run_test(|| async {
            let temp_dir = tempfile::tempdir().unwrap();
            let test_file = temp_dir.path().join("owner-denied.txt");
            std::fs::write(&test_file, "line1\nLOCKED_LINE\nline3").unwrap();

            let read_tool = pi::tools::ReadTool::new(temp_dir.path());
            let line2_tag = get_hashline_tag(&read_tool, &test_file, 2).await;
            let mode_guard = UnixModeGuard::set(&test_file, 0o006);

            let tool = pi::tools::HashlineEditTool::new(temp_dir.path());
            let err = tool
                .execute(
                    "test-id",
                    serde_json::json!({
                        "path": test_file.to_string_lossy(),
                        "edits": [{
                            "op": "replace",
                            "pos": line2_tag,
                            "lines": "CHANGED"
                        }]
                    }),
                    None,
                )
                .await
                .expect_err("hashline_edit must enforce the selected owner mode class");
            let message = err.to_string();
            assert!(
                message.to_lowercase().contains("permission denied"),
                "hashline_edit should reject owner-mode 0: {message}"
            );

            drop(mode_guard);
            assert_eq!(
                std::fs::read_to_string(&test_file).unwrap(),
                "line1\nLOCKED_LINE\nline3",
                "permission failure must leave the file unchanged"
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_parent_without_read_is_rejected_before_hashline_atomic_mutation() {
        asupersync::test_utils::run_test(|| async {
            let harness = TestHarness::new("hashline_parent_without_read");
            let parent = harness.create_dir("write-search-only");
            let path =
                harness.create_file("write-search-only/target.txt", b"line1\nORIGINAL\nline3");
            let read_tool = pi::tools::ReadTool::new(harness.temp_dir());
            let line2_tag = get_hashline_tag(&read_tool, &path, 2).await;
            let mode_guard = UnixModeGuard::set(&parent, 0o300);

            let tool = pi::tools::HashlineEditTool::new(harness.temp_dir());
            let err = tool
                .execute(
                    "test-id",
                    serde_json::json!({
                        "path": path.to_string_lossy(),
                        "edits": [{
                            "op": "replace",
                            "pos": line2_tag,
                            "lines": "CHANGED"
                        }]
                    }),
                    None,
                )
                .await
                .expect_err("durable hashline edit requires opening the parent directory");
            assert!(err.to_string().to_lowercase().contains("permission denied"));

            drop(mode_guard);
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                "line1\nORIGINAL\nline3"
            );
        });
    }

    #[test]
    fn test_anchor_not_found() {
        asupersync::test_utils::run_test(|| async {
            let temp_dir = tempfile::tempdir().unwrap();
            let test_file = temp_dir.path().join("test.txt");
            std::fs::write(&test_file, "line1\nline2\nline3").unwrap();

            let tool = pi::tools::HashlineEditTool::new(temp_dir.path());
            let input = serde_json::json!({
                "path": test_file.to_string_lossy(),
                "edits": [{
                    "op": "replace",
                    "pos": "5#XX",  // Line 5 doesn't exist
                    "lines": "NEW_LINE"
                }]
            });

            let result = tool.execute("test-id", input, None).await;
            assert!(result.is_err());
            let error_msg = result.unwrap_err().to_string();
            assert!(error_msg.contains("Line 5 out of range"));
        });
    }

    #[test]
    fn test_hash_mismatch() {
        asupersync::test_utils::run_test(|| async {
            let temp_dir = tempfile::tempdir().unwrap();
            let test_file = temp_dir.path().join("test.txt");
            std::fs::write(&test_file, "line1\nline2\nline3").unwrap();

            let tool = pi::tools::HashlineEditTool::new(temp_dir.path());
            let input = serde_json::json!({
                "path": test_file.to_string_lossy(),
                "edits": [{
                    "op": "replace",
                    "pos": "2#ZZ",  // Wrong hash for line 2
                    "lines": "NEW_LINE"
                }]
            });

            let result = tool.execute("test-id", input, None).await;
            assert!(result.is_err());
            let error_msg = result.unwrap_err().to_string();
            assert!(error_msg.contains("Hash mismatch at line 2"));
        });
    }

    #[test]
    fn test_empty_file_operations() {
        asupersync::test_utils::run_test(|| async {
            let temp_dir = tempfile::tempdir().unwrap();
            let test_file = temp_dir.path().join("empty.txt");
            std::fs::write(&test_file, "").unwrap();

            let tool = pi::tools::HashlineEditTool::new(temp_dir.path());

            // Test prepend to empty file (BOF)
            let input = serde_json::json!({
                "path": test_file.to_string_lossy(),
                "edits": [{
                    "op": "prepend",
                    "lines": "First line"
                }]
            });

            let _result = tool
                .execute("test-id", input, None)
                .await
                .expect("prepend to empty file should succeed");

            let content = std::fs::read_to_string(&test_file).unwrap();
            assert!(content == "First line\n" || content == "First line");

            // Test append operation
            let input = serde_json::json!({
                "path": test_file.to_string_lossy(),
                "edits": [{
                    "op": "append",
                    "lines": "Second line"
                }]
            });

            let _result = tool
                .execute("test-id", input, None)
                .await
                .expect("append should succeed");

            let content = std::fs::read_to_string(&test_file).unwrap();
            assert_eq!(content.trim_end_matches('\n'), "First line\nSecond line");
        });
    }

    #[test]
    fn test_multiline_edit() {
        asupersync::test_utils::run_test(|| async {
            let temp_dir = tempfile::tempdir().unwrap();
            let test_file = temp_dir.path().join("test.txt");
            std::fs::write(&test_file, "line1\nOLD_LINE\nline3").unwrap();

            // Get the hashline tag for line 2
            let read_tool = pi::tools::ReadTool::new(temp_dir.path());
            let line2_tag = get_hashline_tag(&read_tool, &test_file, 2).await;

            let tool = pi::tools::HashlineEditTool::new(temp_dir.path());
            let input = serde_json::json!({
                "path": test_file.to_string_lossy(),
                "edits": [{
                    "op": "replace",
                    "pos": line2_tag,
                    "lines": ["NEW_LINE_1", "NEW_LINE_2", "NEW_LINE_3"]
                }]
            });

            tool.execute("test-id", input, None)
                .await
                .expect("multiline edit should succeed");

            let content = std::fs::read_to_string(&test_file).unwrap();
            assert_eq!(content, "line1\nNEW_LINE_1\nNEW_LINE_2\nNEW_LINE_3\nline3");
        });
    }

    #[test]
    fn test_unicode_boundaries() {
        asupersync::test_utils::run_test(|| async {
            let temp_dir = tempfile::tempdir().unwrap();
            let test_file = temp_dir.path().join("unicode.txt");
            std::fs::write(&test_file, "αβγ\n🚀🌟💫\nδεζ").unwrap();

            // Get the hashline tag for line 2 (emoji line)
            let read_tool = pi::tools::ReadTool::new(temp_dir.path());
            let line2_tag = get_hashline_tag(&read_tool, &test_file, 2).await;

            let tool = pi::tools::HashlineEditTool::new(temp_dir.path());
            let input = serde_json::json!({
                "path": test_file.to_string_lossy(),
                "edits": [{
                    "op": "replace",
                    "pos": line2_tag,
                    "lines": "⭐️✨🎉"
                }]
            });

            tool.execute("test-id", input, None)
                .await
                .expect("unicode edit should succeed");

            let content = std::fs::read_to_string(&test_file).unwrap();
            assert_eq!(content, "αβγ\n⭐️✨🎉\nδεζ");
        });
    }

    #[test]
    fn test_range_replace() {
        asupersync::test_utils::run_test(|| async {
            let temp_dir = tempfile::tempdir().unwrap();
            let test_file = temp_dir.path().join("range.txt");
            std::fs::write(&test_file, "line1\nSTART\nMIDDLE\nEND\nline5").unwrap();

            // Get the hashline tags for the range
            let read_tool = pi::tools::ReadTool::new(temp_dir.path());
            let start_tag = get_hashline_tag(&read_tool, &test_file, 2).await;
            let end_tag = get_hashline_tag(&read_tool, &test_file, 4).await;

            let tool = pi::tools::HashlineEditTool::new(temp_dir.path());
            let input = serde_json::json!({
                "path": test_file.to_string_lossy(),
                "edits": [{
                    "op": "replace",
                    "pos": start_tag,
                    "end": end_tag,
                    "lines": "REPLACEMENT"
                }]
            });

            tool.execute("test-id", input, None)
                .await
                .expect("range replace should succeed");

            let content = std::fs::read_to_string(&test_file).unwrap();
            assert_eq!(content, "line1\nREPLACEMENT\nline5");
        });
    }
}
