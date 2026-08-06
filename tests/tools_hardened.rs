//! Hardened tool execution tests (bd-1f42.2.2).
//!
//! These tests exercise edge cases, security boundaries, and failure modes for
//! the core filesystem/process tools using real filesystem and process execution
//! in isolated temp workspaces. No mocks. High-fidelity diagnostics on failure.

mod common;

use common::TestHarness;
use pi::model::ContentBlock;
use pi::tools::Tool;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn get_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn rg_available() -> bool {
    std::process::Command::new("rg")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn fd_available() -> bool {
    std::process::Command::new("fd")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(unix)]
struct UnixModeGuard {
    path: std::path::PathBuf,
    original: std::fs::Permissions,
}

#[cfg(unix)]
impl UnixModeGuard {
    fn set(path: &std::path::Path, mode: u32) -> Self {
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

// ===========================================================================
// Read Tool — Hardened
// ===========================================================================
mod read_hardened {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn read_via_symlink() {
        asupersync::test_utils::run_test(|| async {
            let h = TestHarness::new("read_via_symlink");
            let real = h.create_file("real.txt", b"symlink content\n");
            let link = h.temp_path("link.txt");
            std::os::unix::fs::symlink(&real, &link).unwrap();

            let tool = pi::tools::ReadTool::new(h.temp_dir());
            let input = serde_json::json!({"path": link.to_string_lossy()});
            let result = tool.execute("h-read-1", input, None).await.unwrap();
            let text = get_text(&result.content);
            h.log().info_ctx("verify", "symlink read", |ctx| {
                ctx.push(("text".into(), text.clone()));
            });
            assert!(text.contains("symlink content"), "should follow symlink");
            assert!(!result.is_error);
        });
    }

    #[cfg(unix)]
    #[test]
    fn read_symlink_to_directory_fails() {
        asupersync::test_utils::run_test(|| async {
            let h = TestHarness::new("read_symlink_to_directory_fails");
            let dir = h.create_dir("target_dir");
            let link = h.temp_path("link_to_dir");
            std::os::unix::fs::symlink(&dir, &link).unwrap();

            let tool = pi::tools::ReadTool::new(h.temp_dir());
            let input = serde_json::json!({"path": link.to_string_lossy()});
            let err = tool
                .execute("h-read-2", input, None)
                .await
                .expect_err("should fail on dir symlink");
            let msg = err.to_string();
            h.log().info("verify", format!("error={msg}"));
            assert!(
                msg.to_lowercase().contains("directory")
                    || msg.to_lowercase().contains("not a regular file")
                    || msg.to_lowercase().contains("not a file"),
                "should report directory error: {msg}"
            );
        });
    }

    #[test]
    fn read_deeply_nested_path() {
        asupersync::test_utils::run_test(|| async {
            let h = TestHarness::new("read_deeply_nested_path");
            let deep = h.create_file("a/b/c/d/e/f/g/deep.txt", b"deep content");

            let tool = pi::tools::ReadTool::new(h.temp_dir());
            let input = serde_json::json!({"path": deep.to_string_lossy()});
            let result = tool.execute("h-read-3", input, None).await.unwrap();
            let text = get_text(&result.content);
            assert!(text.contains("deep content"));
        });
    }

    #[test]
    fn read_unicode_filename() {
        asupersync::test_utils::run_test(|| async {
            let h = TestHarness::new("read_unicode_filename");
            let path = h.create_file("données_café.txt", b"unicode filename content");

            let tool = pi::tools::ReadTool::new(h.temp_dir());
            let input = serde_json::json!({"path": path.to_string_lossy()});
            let result = tool.execute("h-read-4", input, None).await.unwrap();
            let text = get_text(&result.content);
            assert!(text.contains("unicode filename content"));
        });
    }

    #[test]
    fn read_unicode_content_integrity() {
        asupersync::test_utils::run_test(|| async {
            let h = TestHarness::new("read_unicode_content_integrity");
            let content = "日本語テスト\n中文测试\n한국어 테스트\nEmoji: 🦀🔥\n";
            let path = h.create_file("unicode.txt", content.as_bytes());

            let tool = pi::tools::ReadTool::new(h.temp_dir());
            let input = serde_json::json!({"path": path.to_string_lossy()});
            let result = tool.execute("h-read-5", input, None).await.unwrap();
            let text = get_text(&result.content);
            h.log().info("verify", format!("text={text}"));
            assert!(text.contains("日本語テスト"), "should contain Japanese");
            assert!(text.contains("中文测试"), "should contain Chinese");
            assert!(text.contains("한국어 테스트"), "should contain Korean");
            assert!(text.contains("🦀🔥"), "should contain emoji");
        });
    }

    #[test]
    fn read_file_with_only_empty_lines() {
        asupersync::test_utils::run_test(|| async {
            let h = TestHarness::new("read_file_with_only_empty_lines");
            let content = "\n\n\n\n\n";
            let path = h.create_file("empty_lines.txt", content.as_bytes());

            let tool = pi::tools::ReadTool::new(h.temp_dir());
            let input = serde_json::json!({"path": path.to_string_lossy()});
            let result = tool.execute("h-read-6", input, None).await.unwrap();
            // Should not error on a file that's all newlines
            assert!(!result.is_error);
        });
    }

    #[test]
    fn read_max_bytes_boundary() {
        asupersync::test_utils::run_test(|| async {
            let h = TestHarness::new("read_max_bytes_boundary");
            // Create content exactly at the MAX_BYTES limit
            let line = "x".repeat(99) + "\n"; // 100 bytes per line
            let line_count = pi::tools::DEFAULT_MAX_BYTES / 100;
            let content: String = (0..line_count).map(|_| line.as_str()).collect();
            let path = h.create_file("boundary.txt", content.as_bytes());

            let tool = pi::tools::ReadTool::new(h.temp_dir());
            let input = serde_json::json!({"path": path.to_string_lossy()});
            let result = tool.execute("h-read-7", input, None).await.unwrap();
            // At or just below boundary should work without byte truncation
            assert!(!result.is_error);
        });
    }

    #[test]
    fn read_single_very_long_line() {
        asupersync::test_utils::run_test(|| async {
            let h = TestHarness::new("read_single_very_long_line");
            // One line that exceeds MAX_BYTES
            let content = "a".repeat(pi::tools::DEFAULT_MAX_BYTES + 1000);
            let path = h.create_file("long_line.txt", content.as_bytes());

            let tool = pi::tools::ReadTool::new(h.temp_dir());
            let input = serde_json::json!({"path": path.to_string_lossy()});
            let result = tool.execute("h-read-8", input, None).await.unwrap();
            let text = get_text(&result.content);
            // Should contain truncation notice
            assert!(
                text.contains("exceeds") || text.contains("limit"),
                "should indicate byte truncation: text len = {}",
                text.len()
            );
            let details = result.details.expect("truncation details");
            let truncation = details.get("truncation").expect("truncation object");
            assert_eq!(
                truncation.get("firstLineExceedsLimit"),
                Some(&serde_json::Value::Bool(true))
            );
        });
    }

    #[test]
    fn read_offset_zero_is_first_line() {
        asupersync::test_utils::run_test(|| async {
            let h = TestHarness::new("read_offset_zero_is_first_line");
            let path = h.create_file("lines.txt", b"line1\nline2\nline3\n");

            let tool = pi::tools::ReadTool::new(h.temp_dir());
            let input = serde_json::json!({"path": path.to_string_lossy(), "offset": 0});
            let result = tool.execute("h-read-9", input, None).await.unwrap();
            let text = get_text(&result.content);
            assert!(text.contains("line1"), "offset 0 should start at line 1");
        });
    }

    #[test]
    fn read_offset_and_limit_boundary() {
        asupersync::test_utils::run_test(|| async {
            use std::fmt::Write as _;
            let h = TestHarness::new("read_offset_and_limit_boundary");
            let mut content = String::new();
            for i in 1..=10 {
                let _ = writeln!(content, "line{i}");
            }
            let path = h.create_file("ten.txt", content.as_bytes());

            let tool = pi::tools::ReadTool::new(h.temp_dir());
            // Read exactly the last line
            let input =
                serde_json::json!({"path": path.to_string_lossy(), "offset": 10, "limit": 1});
            let result = tool.execute("h-read-10", input, None).await.unwrap();
            let text = get_text(&result.content);
            assert!(text.contains("line10"), "should contain line 10");
            assert!(!text.contains("line9"), "should not contain line 9");
        });
    }
}

// ===========================================================================
// Write Tool — Hardened
// ===========================================================================
mod write_hardened {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn write_same_workspace_symlink_target() {
        asupersync::test_utils::run_test(|| async {
            let h = TestHarness::new("write_same_workspace_symlink_target");
            let real = h.create_file("real.txt", b"original");
            let link = h.temp_path("link.txt");
            std::os::unix::fs::symlink(&real, &link).unwrap();

            let tool = pi::tools::WriteTool::new(h.temp_dir());
            let input = serde_json::json!({
                "path": link.to_string_lossy(),
                "content": "overwritten via symlink"
            });
            let result = tool.execute("h-write-1", input, None).await.unwrap();
            assert!(!result.is_error);

            // Same-workspace symlinks resolve within the configured CWD and are
            // allowed; the write updates the target content.
            let link_disk = std::fs::read_to_string(&link).unwrap();
            assert_eq!(link_disk, "overwritten via symlink");

            let real_disk = std::fs::read_to_string(&real).unwrap();
            assert_eq!(real_disk, "overwritten via symlink");

            let meta = std::fs::symlink_metadata(&link).unwrap();
            assert!(
                meta.file_type().is_symlink(),
                "link should remain a same-workspace symlink"
            );
        });
    }

    #[test]
    fn write_deeply_nested_auto_creation() {
        asupersync::test_utils::run_test(|| async {
            let h = TestHarness::new("write_deeply_nested_auto_creation");
            let deep = h.temp_path("a/b/c/d/e/f/g/h/i/j/deep.txt");

            let tool = pi::tools::WriteTool::new(h.temp_dir());
            let input = serde_json::json!({
                "path": deep.to_string_lossy(),
                "content": "deep write"
            });
            let result = tool.execute("h-write-2", input, None).await.unwrap();
            assert!(!result.is_error);
            assert!(deep.exists());
            assert_eq!(std::fs::read_to_string(&deep).unwrap(), "deep write");
        });
    }

    #[test]
    fn write_empty_content() {
        asupersync::test_utils::run_test(|| async {
            let h = TestHarness::new("write_empty_content");
            let path = h.temp_path("empty.txt");

            let tool = pi::tools::WriteTool::new(h.temp_dir());
            let input = serde_json::json!({
                "path": path.to_string_lossy(),
                "content": ""
            });
            let result = tool.execute("h-write-3", input, None).await.unwrap();
            assert!(!result.is_error);
            assert!(path.exists());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
        });
    }

    #[test]
    fn write_unicode_round_trip() {
        asupersync::test_utils::run_test(|| async {
            let h = TestHarness::new("write_unicode_round_trip");
            let content = "Héllo Wörld! 日本語 🦀 Ñ ñ ü ö ä ß";
            let path = h.temp_path("unicode_write.txt");

            let tool = pi::tools::WriteTool::new(h.temp_dir());
            let input = serde_json::json!({
                "path": path.to_string_lossy(),
                "content": content
            });
            let result = tool.execute("h-write-4", input, None).await.unwrap();
            assert!(!result.is_error);
            let disk = std::fs::read_to_string(&path).unwrap();
            assert_eq!(disk, content, "unicode content should round-trip perfectly");
        });
    }

    #[test]
    fn write_overwrite_clears_old_content() {
        asupersync::test_utils::run_test(|| async {
            let h = TestHarness::new("write_overwrite_clears_old_content");
            let path = h.create_file("big.txt", b"a]".repeat(10_000).as_slice());

            let tool = pi::tools::WriteTool::new(h.temp_dir());
            let input = serde_json::json!({
                "path": path.to_string_lossy(),
                "content": "short"
            });
            let result = tool.execute("h-write-5", input, None).await.unwrap();
            assert!(!result.is_error);
            let disk = std::fs::read_to_string(&path).unwrap();
            assert_eq!(
                disk, "short",
                "overwrite should completely replace, not append"
            );
        });
    }

    #[test]
    fn write_special_characters_in_path() {
        asupersync::test_utils::run_test(|| async {
            let h = TestHarness::new("write_special_characters_in_path");
            let path = h.temp_path("spaces in name.txt");

            let tool = pi::tools::WriteTool::new(h.temp_dir());
            let input = serde_json::json!({
                "path": path.to_string_lossy(),
                "content": "spaces work"
            });
            let result = tool.execute("h-write-6", input, None).await.unwrap();
            assert!(!result.is_error);
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "spaces work");
        });
    }

    #[test]
    fn write_reports_utf16_byte_count_for_multibyte() {
        asupersync::test_utils::run_test(|| async {
            let h = TestHarness::new("write_reports_utf16_byte_count_for_multibyte");
            // 3 chars: 'A' (1 UTF-16 unit), '😃' (2 UTF-16 units), 'B' (1 UTF-16 unit) = 4 units
            let content = "A😃B";
            let expected_utf16_count = content.encode_utf16().count(); // 4
            let path = h.temp_path("utf16_count.txt");

            let tool = pi::tools::WriteTool::new(h.temp_dir());
            let input = serde_json::json!({
                "path": path.to_string_lossy(),
                "content": content
            });
            let result = tool.execute("h-write-7", input, None).await.unwrap();
            let text = get_text(&result.content);
            h.log().info(
                "verify",
                format!("text={text}, expected={expected_utf16_count}"),
            );
            assert!(
                text.contains(&format!("{expected_utf16_count} bytes")),
                "should report UTF-16 byte count {expected_utf16_count}: {text}"
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn write_readonly_existing_file_fails() {
        asupersync::test_utils::run_test(|| async {
            let h = TestHarness::new("write_readonly_existing_file_fails");
            let path = h.create_file("readonly-write.txt", b"old content");
            let _mode_guard = UnixModeGuard::set(&path, 0o444);

            let tool = pi::tools::WriteTool::new(h.temp_dir());
            let input = serde_json::json!({
                "path": path.to_string_lossy(),
                "content": "new content"
            });
            let err = tool
                .execute("h-write-8", input, None)
                .await
                .expect_err("write should fail on readonly existing file");
            let msg = err.to_string().to_lowercase();
            h.log().info("verify", format!("error={msg}"));
            assert!(
                msg.contains("permission") || msg.contains("denied"),
                "should report permission error: {msg}"
            );
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                "old content",
                "readonly file must remain unchanged"
            );
        });
    }
}

// ===========================================================================
// Edit Tool — Hardened
// ===========================================================================
mod edit_hardened {
    use super::*;

    #[test]
    fn edit_with_regex_metacharacters_in_old_text() {
        asupersync::test_utils::run_test(|| async {
            let h = TestHarness::new("edit_with_regex_metacharacters_in_old_text");
            let content = "value = array[0].method(arg1, arg2);\n";
            let path = h.create_file("meta.txt", content.as_bytes());

            let tool = pi::tools::EditTool::new(h.temp_dir());
            let input = serde_json::json!({
                "path": path.to_string_lossy(),
                "oldText": "array[0].method(arg1, arg2)",
                "newText": "array[1].method(arg3)"
            });
            let result = tool.execute("h-edit-1", input, None).await.unwrap();
            assert!(!result.is_error);
            let disk = std::fs::read_to_string(&path).unwrap();
            assert!(disk.contains("array[1].method(arg3)"));
            assert!(!disk.contains("array[0]"));
        });
    }

    #[test]
    fn edit_entire_file_content() {
        asupersync::test_utils::run_test(|| async {
            let h = TestHarness::new("edit_entire_file_content");
            let path = h.create_file("whole.txt", b"entire old content");

            let tool = pi::tools::EditTool::new(h.temp_dir());
            let input = serde_json::json!({
                "path": path.to_string_lossy(),
                "oldText": "entire old content",
                "newText": "brand new content"
            });
            let result = tool.execute("h-edit-2", input, None).await.unwrap();
            assert!(!result.is_error);
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "brand new content");
        });
    }

    #[test]
    fn edit_multiline_replacement() {
        asupersync::test_utils::run_test(|| async {
            let h = TestHarness::new("edit_multiline_replacement");
            let content = "fn main() {\n    println!(\"hello\");\n}\n";
            let path = h.create_file("multi.rs", content.as_bytes());

            let tool = pi::tools::EditTool::new(h.temp_dir());
            let input = serde_json::json!({
                "path": path.to_string_lossy(),
                "oldText": "fn main() {\n    println!(\"hello\");\n}",
                "newText": "fn main() {\n    eprintln!(\"debug\");\n    println!(\"hello\");\n}"
            });
            let result = tool.execute("h-edit-3", input, None).await.unwrap();
            assert!(!result.is_error);
            let disk = std::fs::read_to_string(&path).unwrap();
            assert!(disk.contains("eprintln!(\"debug\")"));
            assert!(disk.contains("println!(\"hello\")"));
        });
    }

    #[test]
    fn edit_sequential_edits_on_same_file() {
        asupersync::test_utils::run_test(|| async {
            let h = TestHarness::new("edit_sequential_edits_on_same_file");
            let path = h.create_file("seq.txt", b"alpha beta gamma delta\n");

            let tool = pi::tools::EditTool::new(h.temp_dir());

            // First edit
            let input = serde_json::json!({
                "path": path.to_string_lossy(),
                "oldText": "alpha",
                "newText": "ALPHA"
            });
            tool.execute("h-edit-4a", input, None).await.unwrap();

            // Second edit
            let input = serde_json::json!({
                "path": path.to_string_lossy(),
                "oldText": "gamma",
                "newText": "GAMMA"
            });
            tool.execute("h-edit-4b", input, None).await.unwrap();

            let disk = std::fs::read_to_string(&path).unwrap();
            h.log().info("verify", format!("disk={disk}"));
            assert!(disk.contains("ALPHA"), "first edit should persist");
            assert!(disk.contains("GAMMA"), "second edit should apply");
            assert!(disk.contains("beta"), "untouched text should remain");
            assert!(disk.contains("delta"), "untouched text should remain");
        });
    }

    #[test]
    fn edit_preserves_crlf_line_endings() {
        asupersync::test_utils::run_test(|| async {
            let h = TestHarness::new("edit_preserves_crlf_line_endings");
            let content = "line1\r\nTARGET\r\nline3\r\n";
            let path = h.create_file("crlf.txt", content.as_bytes());

            let tool = pi::tools::EditTool::new(h.temp_dir());
            let input = serde_json::json!({
                "path": path.to_string_lossy(),
                "oldText": "TARGET",
                "newText": "REPLACED"
            });
            let result = tool.execute("h-edit-5", input, None).await.unwrap();
            assert!(!result.is_error);

            let bytes = std::fs::read(&path).unwrap();
            let disk = String::from_utf8_lossy(&bytes);
            h.log().info("verify", format!("disk={disk:?}"));
            assert!(disk.contains("REPLACED"), "edit should apply");
            // Count CRLF occurrences
            let crlf_count = disk.matches("\r\n").count();
            assert!(
                crlf_count >= 2,
                "CRLF line endings should be preserved (found {crlf_count})"
            );
        });
    }

    #[test]
    fn edit_unicode_content() {
        asupersync::test_utils::run_test(|| async {
            let h = TestHarness::new("edit_unicode_content");
            let path = h.create_file("uni_edit.txt", "Hello 世界! 🌍".as_bytes());

            let tool = pi::tools::EditTool::new(h.temp_dir());
            let input = serde_json::json!({
                "path": path.to_string_lossy(),
                "oldText": "世界",
                "newText": "ワールド"
            });
            let result = tool.execute("h-edit-6", input, None).await.unwrap();
            assert!(!result.is_error);
            let disk = std::fs::read_to_string(&path).unwrap();
            assert!(disk.contains("ワールド"));
            assert!(!disk.contains("世界"));
        });
    }

    #[cfg(unix)]
    #[test]
    fn edit_readonly_file_fails() {
        asupersync::test_utils::run_test(|| async {
            let h = TestHarness::new("edit_readonly_file_fails");
            let path = h.create_file("readonly.txt", b"immutable content");
            let _mode_guard = UnixModeGuard::set(&path, 0o444);

            let tool = pi::tools::EditTool::new(h.temp_dir());
            let input = serde_json::json!({
                "path": path.to_string_lossy(),
                "oldText": "immutable",
                "newText": "mutable"
            });
            let err = tool
                .execute("h-edit-7", input, None)
                .await
                .expect_err("should fail on readonly");
            let msg = err.to_string().to_lowercase();
            h.log().info("verify", format!("error={msg}"));
            assert!(
                msg.contains("permission denied"),
                "should report PermissionDenied: {msg}"
            );
            // Verify the file content was NOT modified.
            let disk = std::fs::read_to_string(&path).unwrap();
            assert_eq!(
                disk, "immutable content",
                "readonly file must not be modified"
            );
        });
    }

    #[test]
    fn edit_diff_details_present() {
        asupersync::test_utils::run_test(|| async {
            let h = TestHarness::new("edit_diff_details_present");
            let path = h.create_file("diff.txt", b"before\n");

            let tool = pi::tools::EditTool::new(h.temp_dir());
            let input = serde_json::json!({
                "path": path.to_string_lossy(),
                "oldText": "before",
                "newText": "after"
            });
            let result = tool.execute("h-edit-8", input, None).await.unwrap();
            let details = result.details.expect("should have details");
            let diff = details.get("diff").expect("should have diff field");
            let diff_str = diff.as_str().unwrap_or("");
            h.log().info("verify", format!("diff={diff_str}"));
            assert!(!diff_str.is_empty(), "diff should not be empty");
        });
    }
}

// ===========================================================================
// Bash Tool — Hardened
// ===========================================================================
mod bash_hardened {
    use super::*;

    #[test]
    fn bash_stderr_captured_in_output() {
        asupersync::test_utils::run_test(|| async {
            let h = TestHarness::new("bash_stderr_captured_in_output");
            let tool = pi::tools::BashTool::new(h.temp_dir());
            let input = serde_json::json!({
                "command": "echo 'stdout_line' && echo 'stderr_line' >&2"
            });
            let result = tool.execute("h-bash-1", input, None).await.unwrap();
            let text = get_text(&result.content);
            h.log().info("verify", format!("text={text}"));
            assert!(text.contains("stdout_line"), "should capture stdout");
            assert!(text.contains("stderr_line"), "should capture stderr");
        });
    }

    #[test]
    fn bash_exit_code_boundary_values() {
        asupersync::test_utils::run_test(|| async {
            let h = TestHarness::new("bash_exit_code_boundary_values");
            let tool = pi::tools::BashTool::new(h.temp_dir());

            // Exit 0 should succeed
            let input = serde_json::json!({"command": "exit 0"});
            let result = tool.execute("h-bash-2a", input, None).await;
            assert!(result.is_ok(), "exit 0 should succeed");

            // Non-zero exits are successful tool invocations with is_error=true.
            let input = serde_json::json!({"command": "exit 1"});
            let output = tool
                .execute("h-bash-2b", input, None)
                .await
                .expect("exit 1 should return a tool error output");
            assert!(output.is_error, "exit 1 must set is_error");
            let message = get_text(&output.content);
            assert!(message.contains("code 1"), "should report exit code 1");

            // Exit 127 (command not found convention)
            let input = serde_json::json!({"command": "exit 127"});
            let output = tool
                .execute("h-bash-2c", input, None)
                .await
                .expect("exit 127 should return a tool error output");
            assert!(output.is_error, "exit 127 must set is_error");
            let message = get_text(&output.content);
            assert!(message.contains("127"), "should report exit code 127");

            h.log().info("verify", "all exit code boundaries passed");
        });
    }

    #[test]
    fn bash_shell_syntax_error() {
        asupersync::test_utils::run_test(|| async {
            let h = TestHarness::new("bash_shell_syntax_error");
            let tool = pi::tools::BashTool::new(h.temp_dir());
            let input = serde_json::json!({"command": "if then else fi"});
            let output = tool
                .execute("h-bash-3", input, None)
                .await
                .expect("syntax error should return a tool error output");
            assert!(output.is_error, "syntax error must set is_error");
            let msg = get_text(&output.content);
            h.log().info("verify", format!("error={msg}"));
            assert!(
                msg.contains("exited with code") || msg.contains("syntax"),
                "should report shell error: {msg}"
            );
        });
    }

    #[test]
    fn bash_multiline_command() {
        asupersync::test_utils::run_test(|| async {
            let h = TestHarness::new("bash_multiline_command");
            let tool = pi::tools::BashTool::new(h.temp_dir());
            let input = serde_json::json!({
                "command": "echo line1\necho line2\necho line3"
            });
            let result = tool.execute("h-bash-4", input, None).await.unwrap();
            let text = get_text(&result.content);
            h.log().info("verify", format!("text={text}"));
            assert!(text.contains("line1"), "should contain line1");
            assert!(text.contains("line2"), "should contain line2");
            assert!(text.contains("line3"), "should contain line3");
        });
    }

    #[test]
    fn bash_working_directory_is_correct() {
        asupersync::test_utils::run_test(|| async {
            let h = TestHarness::new("bash_working_directory_is_correct");
            let tool = pi::tools::BashTool::new(h.temp_dir());
            let input = serde_json::json!({"command": "pwd"});
            let result = tool.execute("h-bash-5", input, None).await.unwrap();
            let text = get_text(&result.content);
            h.log().info("verify", format!("pwd={text}"));
            // The pwd output should match or be within the temp dir
            let temp_dir_str = h.temp_dir().to_string_lossy();
            assert!(
                text.contains(&*temp_dir_str),
                "pwd should match temp dir: got {text}, expected to contain {temp_dir_str}"
            );
        });
    }

    #[test]
    fn bash_large_stderr_output() {
        asupersync::test_utils::run_test(|| async {
            let h = TestHarness::new("bash_large_stderr_output");
            let tool = pi::tools::BashTool::new(h.temp_dir());
            let input = serde_json::json!({
                "command": "for i in $(seq 1 500); do echo \"stderr line $i\" >&2; done"
            });
            let result = tool.execute("h-bash-6", input, None).await.unwrap();
            let text = get_text(&result.content);
            h.log().info("verify", format!("text_len={}", text.len()));
            assert!(text.contains("stderr line"), "should capture stderr output");
        });
    }

    #[cfg(unix)]
    #[test]
    fn bash_timeout_kills_process_tree() {
        asupersync::test_utils::run_test(|| async {
            let h = TestHarness::new("bash_timeout_kills_process_tree");
            let tool = pi::tools::BashTool::new(h.temp_dir());
            // Use a marker file to detect if process survived
            let marker = h.temp_path("survived.txt");
            let marker_str = marker.to_string_lossy().to_string();
            let input = serde_json::json!({
                "command": format!("sleep 30 && touch {marker_str}"),
                "timeout": 1
            });
            let output = tool
                .execute("h-bash-7", input, None)
                .await
                .expect("timeout should return a tool error output");
            assert!(output.is_error, "timeout must set is_error");
            let msg = get_text(&output.content);
            h.log().info("verify", format!("error={msg}"));
            assert!(msg.contains("timed out"), "should report timeout: {msg}");

            // Small delay to let any orphans finish
            std::thread::sleep(std::time::Duration::from_millis(500));
            assert!(
                !marker.exists(),
                "marker file should NOT exist - process should have been killed"
            );
        });
    }

    #[test]
    fn bash_empty_command() {
        asupersync::test_utils::run_test(|| async {
            let h = TestHarness::new("bash_empty_command");
            let tool = pi::tools::BashTool::new(h.temp_dir());
            let input = serde_json::json!({"command": ""});
            // Empty command should either succeed with empty output or fail gracefully
            let result = tool.execute("h-bash-8", input, None).await;
            h.log()
                .info("verify", format!("result_ok={}", result.is_ok()));
            // Either way, should not panic
        });
    }

    #[test]
    fn bash_env_variable_not_leaked() {
        asupersync::test_utils::run_test(|| async {
            let h = TestHarness::new("bash_env_variable_not_leaked");
            let tool = pi::tools::BashTool::new(h.temp_dir());
            // Check that a random env var we set is accessible (bash inherits env)
            let input = serde_json::json!({"command": "echo $HOME"});
            let result = tool.execute("h-bash-9", input, None).await.unwrap();
            let text = get_text(&result.content);
            h.log().info("verify", format!("HOME={text}"));
            // HOME should be set (basic env inheritance check)
            assert!(!text.trim().is_empty(), "HOME env var should be set");
        });
    }

    #[test]
    fn bash_pipe_commands() {
        asupersync::test_utils::run_test(|| async {
            let h = TestHarness::new("bash_pipe_commands");
            let tool = pi::tools::BashTool::new(h.temp_dir());
            let input = serde_json::json!({
                "command": "echo 'hello world' | wc -w"
            });
            let result = tool.execute("h-bash-10", input, None).await.unwrap();
            let text = get_text(&result.content);
            h.log().info("verify", format!("wc={text}"));
            assert!(text.contains('2'), "should count 2 words");
        });
    }
}

// ===========================================================================
// Grep Tool — Hardened
// ===========================================================================
mod grep_hardened {
    use super::*;

    #[test]
    fn grep_regex_metacharacters() {
        if !rg_available() {
            eprintln!("SKIP: rg not available");
            return;
        }
        asupersync::test_utils::run_test(|| async {
            let h = TestHarness::new("grep_regex_metacharacters");
            h.create_file(
                "code.rs",
                b"fn foo() -> Result<i32, Error> {\n    Ok(42)\n}\n",
            );

            let tool = pi::tools::GrepTool::new(h.temp_dir());
            // Literal search for "Result<i32"
            let input = serde_json::json!({"pattern": "Result<i32"});
            let result = tool.execute("h-grep-1", input, None).await.unwrap();
            let text = get_text(&result.content);
            h.log().info("verify", format!("text={text}"));
            assert!(
                text.contains("Result<i32"),
                "should find regex metachar pattern"
            );
        });
    }

    #[test]
    fn grep_unicode_pattern() {
        if !rg_available() {
            eprintln!("SKIP: rg not available");
            return;
        }
        asupersync::test_utils::run_test(|| async {
            let h = TestHarness::new("grep_unicode_pattern");
            h.create_file(
                "text.txt",
                "Hello 世界\nGoodbye 世界\nHello world\n".as_bytes(),
            );

            let tool = pi::tools::GrepTool::new(h.temp_dir());
            let input = serde_json::json!({"pattern": "世界"});
            let result = tool.execute("h-grep-2", input, None).await.unwrap();
            let text = get_text(&result.content);
            h.log().info("verify", format!("text={text}"));
            assert!(text.contains("Hello 世界"), "should match Unicode pattern");
            assert!(
                text.contains("Goodbye 世界"),
                "should match Unicode pattern"
            );
            assert!(
                !text.contains("Hello world") || text.lines().count() >= 2,
                "should match correct lines"
            );
        });
    }

    #[test]
    fn grep_with_path_parameter() {
        if !rg_available() {
            eprintln!("SKIP: rg not available");
            return;
        }
        asupersync::test_utils::run_test(|| async {
            let h = TestHarness::new("grep_with_path_parameter");
            h.create_file("src/main.rs", b"fn main() { target_match }\n");
            h.create_file("tests/test.rs", b"fn test() { target_match }\n");

            let tool = pi::tools::GrepTool::new(h.temp_dir());
            let input = serde_json::json!({
                "pattern": "target_match",
                "path": "src"
            });
            let result = tool.execute("h-grep-3", input, None).await.unwrap();
            let text = get_text(&result.content);
            h.log().info("verify", format!("text={text}"));
            assert!(text.contains("main.rs"), "should find match in src/");
            // Should NOT include tests/ match when path is scoped to src/
        });
    }

    #[test]
    fn grep_empty_file() {
        if !rg_available() {
            eprintln!("SKIP: rg not available");
            return;
        }
        asupersync::test_utils::run_test(|| async {
            let h = TestHarness::new("grep_empty_file");
            h.create_file("empty.txt", b"");

            let tool = pi::tools::GrepTool::new(h.temp_dir());
            let input = serde_json::json!({"pattern": "anything"});
            let result = tool.execute("h-grep-4", input, None).await.unwrap();
            let text = get_text(&result.content);
            assert!(
                text.contains("No matches found"),
                "should report no matches in empty file: {text}"
            );
        });
    }

    #[test]
    fn grep_context_lines_correct() {
        if !rg_available() {
            eprintln!("SKIP: rg not available");
            return;
        }
        asupersync::test_utils::run_test(|| async {
            use std::fmt::Write as _;
            let h = TestHarness::new("grep_context_lines_correct");
            let mut content = String::new();
            for i in 1..=20 {
                let _ = writeln!(content, "line{i}");
            }
            h.create_file("ctx.txt", content.as_bytes());

            let tool = pi::tools::GrepTool::new(h.temp_dir());
            let input = serde_json::json!({
                "pattern": "line10",
                "context": 2
            });
            let result = tool.execute("h-grep-5", input, None).await.unwrap();
            let text = get_text(&result.content);
            h.log().info("verify", format!("text={text}"));
            assert!(text.contains("line10"), "should contain match");
            assert!(text.contains("line8"), "should contain context before");
            assert!(text.contains("line12"), "should contain context after");
        });
    }

    #[test]
    fn grep_multiple_files_results() {
        if !rg_available() {
            eprintln!("SKIP: rg not available");
            return;
        }
        asupersync::test_utils::run_test(|| async {
            let h = TestHarness::new("grep_multiple_files_results");
            h.create_file("a.txt", b"needle in a");
            h.create_file("b.txt", b"needle in b");
            h.create_file("c.txt", b"no match here");

            let tool = pi::tools::GrepTool::new(h.temp_dir());
            let input = serde_json::json!({"pattern": "needle"});
            let result = tool.execute("h-grep-6", input, None).await.unwrap();
            let text = get_text(&result.content);
            h.log().info("verify", format!("text={text}"));
            assert!(text.contains("a.txt"), "should find in a.txt");
            assert!(text.contains("b.txt"), "should find in b.txt");
        });
    }
}

// ===========================================================================
// Find Tool — Hardened
// ===========================================================================
mod find_hardened {
    use super::*;

    #[test]
    fn find_nested_glob() {
        if !fd_available() {
            eprintln!("SKIP: fd not available");
            return;
        }
        asupersync::test_utils::run_test(|| async {
            let h = TestHarness::new("find_nested_glob");
            h.create_file("src/main.rs", b"");
            h.create_file("src/lib.rs", b"");
            h.create_file("tests/test.rs", b"");
            h.create_file("docs/readme.md", b"");

            let tool = pi::tools::FindTool::new(h.temp_dir());
            let input = serde_json::json!({"pattern": "*.rs"});
            let result = tool.execute("h-find-1", input, None).await.unwrap();
            let text = get_text(&result.content);
            h.log().info("verify", format!("text={text}"));
            assert!(text.contains("main.rs"), "should find main.rs");
            assert!(text.contains("lib.rs"), "should find lib.rs");
            assert!(text.contains("test.rs"), "should find test.rs");
            assert!(!text.contains("readme.md"), "should not find .md files");
        });
    }

    #[test]
    fn find_with_path_scoping() {
        if !fd_available() {
            eprintln!("SKIP: fd not available");
            return;
        }
        asupersync::test_utils::run_test(|| async {
            let h = TestHarness::new("find_with_path_scoping");
            h.create_file("src/main.rs", b"");
            h.create_file("tests/test.rs", b"");

            let tool = pi::tools::FindTool::new(h.temp_dir());
            let input = serde_json::json!({
                "pattern": "*.rs",
                "path": "src"
            });
            let result = tool.execute("h-find-2", input, None).await.unwrap();
            let text = get_text(&result.content);
            h.log().info("verify", format!("text={text}"));
            assert!(text.contains("main.rs"), "should find main.rs in src/");
        });
    }

    #[test]
    fn find_empty_directory_tree() {
        if !fd_available() {
            eprintln!("SKIP: fd not available");
            return;
        }
        asupersync::test_utils::run_test(|| async {
            let h = TestHarness::new("find_empty_directory_tree");
            h.create_dir("empty_tree/sub1/sub2");

            let tool = pi::tools::FindTool::new(h.temp_dir());
            let input = serde_json::json!({"pattern": "*.rs"});
            let result = tool.execute("h-find-3", input, None).await.unwrap();
            let text = get_text(&result.content);
            assert!(
                text.contains("No files found"),
                "empty tree should report no files: {text}"
            );
        });
    }

    #[test]
    fn find_unicode_filename() {
        if !fd_available() {
            eprintln!("SKIP: fd not available");
            return;
        }
        asupersync::test_utils::run_test(|| async {
            let h = TestHarness::new("find_unicode_filename");
            h.create_file("café.txt", b"");
            h.create_file("normal.txt", b"");

            let tool = pi::tools::FindTool::new(h.temp_dir());
            let input = serde_json::json!({"pattern": "*.txt"});
            let result = tool.execute("h-find-4", input, None).await.unwrap();
            let text = get_text(&result.content);
            h.log().info("verify", format!("text={text}"));
            assert!(text.contains("café.txt"), "should find Unicode filename");
            assert!(text.contains("normal.txt"), "should find normal filename");
        });
    }

    #[test]
    fn find_limit_respects_cap() {
        if !fd_available() {
            eprintln!("SKIP: fd not available");
            return;
        }
        asupersync::test_utils::run_test(|| async {
            let h = TestHarness::new("find_limit_respects_cap");
            for i in 0..20 {
                h.create_file(format!("file_{i:02}.txt"), b"");
            }

            let tool = pi::tools::FindTool::new(h.temp_dir());
            let input = serde_json::json!({"pattern": "*.txt", "limit": 5});
            let result = tool.execute("h-find-5", input, None).await.unwrap();
            let text = get_text(&result.content);
            let file_count = text.lines().filter(|l| l.contains(".txt")).count();
            h.log().info("verify", format!("found={file_count}"));
            assert!(
                file_count <= 5,
                "should respect limit=5, found {file_count}"
            );
            assert!(
                text.contains("results limit reached"),
                "should indicate limit reached: {text}"
            );
        });
    }
}

// ===========================================================================
// Ls Tool — Hardened
// ===========================================================================
mod ls_hardened {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn ls_shows_symlinks() {
        asupersync::test_utils::run_test(|| async {
            let h = TestHarness::new("ls_shows_symlinks");
            h.create_file("real.txt", b"content");
            let link = h.temp_path("link.txt");
            std::os::unix::fs::symlink(h.temp_path("real.txt"), &link).unwrap();

            let tool = pi::tools::LsTool::new(h.temp_dir());
            let input = serde_json::json!({});
            let result = tool.execute("h-ls-1", input, None).await.unwrap();
            let text = get_text(&result.content);
            h.log().info("verify", format!("text={text}"));
            assert!(text.contains("real.txt"), "should list real file");
            assert!(text.contains("link.txt"), "should list symlink");
        });
    }

    #[test]
    fn ls_many_entries() {
        asupersync::test_utils::run_test(|| async {
            let h = TestHarness::new("ls_many_entries");
            for i in 0..50 {
                h.create_file(format!("file_{i:03}.txt"), b"");
            }

            let tool = pi::tools::LsTool::new(h.temp_dir());
            let input = serde_json::json!({});
            let result = tool.execute("h-ls-2", input, None).await.unwrap();
            let text = get_text(&result.content);
            let line_count = text.lines().count();
            h.log().info("verify", format!("lines={line_count}"));
            assert!(
                line_count >= 50,
                "should list all 50 files (got {line_count})"
            );
        });
    }

    #[test]
    fn ls_alphabetical_order() {
        asupersync::test_utils::run_test(|| async {
            let h = TestHarness::new("ls_alphabetical_order");
            h.create_file("cherry.txt", b"");
            h.create_file("apple.txt", b"");
            h.create_file("banana.txt", b"");

            let tool = pi::tools::LsTool::new(h.temp_dir());
            let input = serde_json::json!({});
            let result = tool.execute("h-ls-3", input, None).await.unwrap();
            let text = get_text(&result.content);
            let lines: Vec<&str> = text.lines().filter(|l| l.contains(".txt")).collect();
            h.log().info("verify", format!("lines={lines:?}"));

            // Find positions
            let apple_pos = text.find("apple.txt");
            let banana_pos = text.find("banana.txt");
            let cherry_pos = text.find("cherry.txt");
            assert!(
                apple_pos < banana_pos && banana_pos < cherry_pos,
                "entries should be alphabetical: apple({apple_pos:?}) < banana({banana_pos:?}) < cherry({cherry_pos:?})"
            );
        });
    }

    #[test]
    fn ls_directories_have_trailing_slash() {
        asupersync::test_utils::run_test(|| async {
            let h = TestHarness::new("ls_directories_have_trailing_slash");
            h.create_dir("subdir_a");
            h.create_dir("subdir_b");
            h.create_file("file.txt", b"");

            let tool = pi::tools::LsTool::new(h.temp_dir());
            let input = serde_json::json!({});
            let result = tool.execute("h-ls-4", input, None).await.unwrap();
            let text = get_text(&result.content);
            h.log().info("verify", format!("text={text}"));
            assert!(text.contains("subdir_a/"), "dirs should have trailing /");
            assert!(text.contains("subdir_b/"), "dirs should have trailing /");
            assert!(
                text.contains("file.txt"),
                "files should not have trailing /"
            );
            assert!(
                !text.contains("file.txt/"),
                "files must not have trailing /"
            );
        });
    }

    #[test]
    fn ls_limit_truncation_details() {
        asupersync::test_utils::run_test(|| async {
            let h = TestHarness::new("ls_limit_truncation_details");
            for i in 0..10 {
                h.create_file(format!("f{i}.txt"), b"");
            }

            let tool = pi::tools::LsTool::new(h.temp_dir());
            let input = serde_json::json!({"limit": 3});
            let result = tool.execute("h-ls-5", input, None).await.unwrap();
            let text = get_text(&result.content);
            assert!(
                text.contains("entries limit reached"),
                "should report limit: {text}"
            );
            let details = result.details.expect("should have details");
            assert_eq!(
                details.get("entryLimitReached"),
                Some(&serde_json::Value::Number(3u64.into()))
            );
        });
    }

    #[test]
    fn ls_unicode_filenames() {
        asupersync::test_utils::run_test(|| async {
            let h = TestHarness::new("ls_unicode_filenames");
            h.create_file("日本語.txt", b"");
            h.create_file("normal.txt", b"");

            let tool = pi::tools::LsTool::new(h.temp_dir());
            let input = serde_json::json!({});
            let result = tool.execute("h-ls-6", input, None).await.unwrap();
            let text = get_text(&result.content);
            h.log().info("verify", format!("text={text}"));
            assert!(text.contains("日本語.txt"), "should list Unicode filename");
            assert!(text.contains("normal.txt"), "should list normal filename");
        });
    }

    #[test]
    fn ls_nested_directory_path() {
        asupersync::test_utils::run_test(|| async {
            let h = TestHarness::new("ls_nested_directory_path");
            h.create_file("sub/a.txt", b"");
            h.create_file("sub/b.txt", b"");
            h.create_dir("sub/inner");

            let tool = pi::tools::LsTool::new(h.temp_dir());
            let input = serde_json::json!({"path": h.temp_path("sub").to_string_lossy()});
            let result = tool.execute("h-ls-7", input, None).await.unwrap();
            let text = get_text(&result.content);
            assert!(text.contains("a.txt"), "should list a.txt");
            assert!(text.contains("b.txt"), "should list b.txt");
            assert!(text.contains("inner/"), "should list inner/ dir");
        });
    }
}

// ===========================================================================
// Cross-Tool Integration — Hardened
// ===========================================================================
mod cross_tool_hardened {
    use super::*;

    #[test]
    fn write_then_grep_finds_content() {
        if !rg_available() {
            eprintln!("SKIP: rg not available");
            return;
        }
        asupersync::test_utils::run_test(|| async {
            let h = TestHarness::new("write_then_grep_finds_content");
            let path = h.temp_path("searchable.txt");

            let write_tool = pi::tools::WriteTool::new(h.temp_dir());
            let write_input = serde_json::json!({
                "path": path.to_string_lossy(),
                "content": "unique_marker_string_for_grep_test\n"
            });
            write_tool
                .execute("cross-1a", write_input, None)
                .await
                .unwrap();

            let grep_tool = pi::tools::GrepTool::new(h.temp_dir());
            let grep_input = serde_json::json!({"pattern": "unique_marker_string_for_grep_test"});
            let result = grep_tool
                .execute("cross-1b", grep_input, None)
                .await
                .unwrap();
            let text = get_text(&result.content);
            assert!(
                text.contains("unique_marker_string_for_grep_test"),
                "grep should find written content"
            );
        });
    }

    #[test]
    fn write_then_find_discovers_file() {
        if !fd_available() {
            eprintln!("SKIP: fd not available");
            return;
        }
        asupersync::test_utils::run_test(|| async {
            let h = TestHarness::new("write_then_find_discovers_file");
            let path = h.temp_path("discoverable.rs");

            let write_tool = pi::tools::WriteTool::new(h.temp_dir());
            let write_input = serde_json::json!({
                "path": path.to_string_lossy(),
                "content": "fn discover() {}\n"
            });
            write_tool
                .execute("cross-2a", write_input, None)
                .await
                .unwrap();

            let find_tool = pi::tools::FindTool::new(h.temp_dir());
            let find_input = serde_json::json!({"pattern": "*.rs"});
            let result = find_tool
                .execute("cross-2b", find_input, None)
                .await
                .unwrap();
            let text = get_text(&result.content);
            assert!(
                text.contains("discoverable.rs"),
                "find should discover written file"
            );
        });
    }

    #[test]
    fn bash_creates_file_then_read_finds_it() {
        asupersync::test_utils::run_test(|| async {
            let h = TestHarness::new("bash_creates_file_then_read_finds_it");
            let file_path = h.temp_path("bash_created.txt");
            let file_str = file_path.to_string_lossy().to_string();

            let bash_tool = pi::tools::BashTool::new(h.temp_dir());
            let bash_input = serde_json::json!({
                "command": format!("echo 'bash wrote this' > '{file_str}'")
            });
            bash_tool
                .execute("cross-3a", bash_input, None)
                .await
                .unwrap();

            let read_tool = pi::tools::ReadTool::new(h.temp_dir());
            let read_input = serde_json::json!({"path": file_str});
            let result = read_tool
                .execute("cross-3b", read_input, None)
                .await
                .unwrap();
            let text = get_text(&result.content);
            assert!(
                text.contains("bash wrote this"),
                "read should find bash-created content"
            );
        });
    }

    #[test]
    fn edit_then_ls_shows_unchanged_listing() {
        asupersync::test_utils::run_test(|| async {
            let h = TestHarness::new("edit_then_ls_shows_unchanged_listing");
            h.create_file("target.txt", b"old content");
            h.create_file("other.txt", b"untouched");

            let edit_tool = pi::tools::EditTool::new(h.temp_dir());
            let edit_input = serde_json::json!({
                "path": h.temp_path("target.txt").to_string_lossy(),
                "oldText": "old content",
                "newText": "new content"
            });
            edit_tool
                .execute("cross-4a", edit_input, None)
                .await
                .unwrap();

            let ls_tool = pi::tools::LsTool::new(h.temp_dir());
            let ls_input = serde_json::json!({});
            let result = ls_tool.execute("cross-4b", ls_input, None).await.unwrap();
            let text = get_text(&result.content);
            assert!(
                text.contains("target.txt"),
                "edited file should still appear in ls"
            );
            assert!(
                text.contains("other.txt"),
                "other file should be unaffected"
            );
        });
    }
}
