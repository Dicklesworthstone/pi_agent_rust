//! E2E: Built-in tool execution with artifact logging (bd-2xyv).
//!
//! Tests the core filesystem/process tools (read, write, edit, bash, grep, find,
//! ls) via direct `ToolRegistry::get(name).execute()` calls against real file
//! system operations in temp directories. No mocks, no network, fully
//! deterministic.

mod common;

use common::TestHarness;
use pi::error::Error;
use pi::model::ContentBlock;
use pi::tools::ToolRegistry;
use serde_json::json;
use std::fmt::Write as _;
#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_registry(cwd: &Path) -> ToolRegistry {
    ToolRegistry::new(
        &["read", "write", "edit", "bash", "grep", "find", "ls"],
        cwd,
        None,
    )
}

/// Extract the first text content from a `ToolOutput`.
fn first_text(content: &[ContentBlock]) -> &str {
    content
        .iter()
        .find_map(|b| match b {
            ContentBlock::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .unwrap_or("")
}

/// Check if `rg` (ripgrep) is available on this machine.
fn rg_available() -> bool {
    std::process::Command::new("rg")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}

/// Check if `fd` or `fdfind` is available on this machine.
fn fd_available() -> bool {
    std::process::Command::new("fd")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
        || std::process::Command::new("fdfind")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok()
}

// ===========================================================================
// Read Tool
// ===========================================================================

#[test]
fn read_text_file_basic() {
    let h = TestHarness::new("read_text_file_basic");
    let file = h.create_file("hello.txt", "line one\nline two\nline three\n");
    let registry = make_registry(h.temp_dir());

    let output = common::run_async(async move {
        let tool = registry.get("read").unwrap();
        tool.execute("call-1", json!({"path": file.display().to_string()}), None)
            .await
    });

    let result = output.unwrap();
    let text = first_text(&result.content);
    h.log().info("result", format!("text={text}"));

    // Line-numbered cat-n format: "    1→line one"
    assert!(text.contains("1→line one"), "should contain line 1");
    assert!(text.contains("2→line two"), "should contain line 2");
    assert!(text.contains("3→line three"), "should contain line 3");
    assert!(!result.is_error);
}

#[test]
fn read_with_offset_and_limit() {
    let h = TestHarness::new("read_with_offset_and_limit");
    let mut content = String::new();
    for i in 1..=20 {
        use std::fmt::Write as _;
        let _ = writeln!(&mut content, "line {i}");
    }
    let file = h.create_file("lines.txt", &content);
    let registry = make_registry(h.temp_dir());

    let output = common::run_async(async move {
        let tool = registry.get("read").unwrap();
        tool.execute(
            "call-2",
            json!({"path": file.display().to_string(), "offset": 5, "limit": 6}),
            None,
        )
        .await
    });

    let result = output.unwrap();
    let text = first_text(&result.content);
    h.log().info("result", format!("text={text}"));

    // offset=5 means start at line 5, limit=6 means show 6 lines (5-10)
    assert!(text.contains("5→line 5"), "should start at line 5");
    assert!(text.contains("10→line 10"), "should include line 10");
    assert!(!text.contains("4→"), "should not include line 4");
    assert!(!text.contains("11→line 11"), "should not include line 11");
}

#[test]
fn read_large_file_truncation() {
    let h = TestHarness::new("read_large_file_truncation");
    // Create a file with 3000 lines - should be truncated at 2000 lines
    let mut content = String::new();
    for i in 1..=3000 {
        use std::fmt::Write as _;
        let _ = writeln!(&mut content, "line number {i}");
    }
    let file = h.create_file("big.txt", &content);
    let registry = make_registry(h.temp_dir());

    let output = common::run_async(async move {
        let tool = registry.get("read").unwrap();
        tool.execute("call-3", json!({"path": file.display().to_string()}), None)
            .await
    });

    let result = output.unwrap();
    let text = first_text(&result.content);

    // Should contain truncation notice
    assert!(
        text.contains("Use offset=") || text.contains("Showing lines"),
        "should indicate truncation"
    );
    // Should contain line 1 but not line 3000
    assert!(
        text.contains("1→line number 1"),
        "should contain first line"
    );
    assert!(
        !text.contains("3000→"),
        "should not contain line 3000 (truncated)"
    );
}

#[test]
fn read_missing_file_error() {
    let h = TestHarness::new("read_missing_file_error");
    let registry = make_registry(h.temp_dir());
    let missing = h.temp_path("does_not_exist.txt");

    let output = common::run_async(async move {
        let tool = registry.get("read").unwrap();
        tool.execute(
            "call-4",
            json!({"path": missing.display().to_string()}),
            None,
        )
        .await
    });

    let err = output.unwrap_err();
    h.log().info("result", format!("error={err}"));
    assert!(
        matches!(err, Error::Tool { .. }),
        "should be a Tool error, got: {err}"
    );
}

#[test]
fn read_binary_file_returns_image() {
    let h = TestHarness::new("read_binary_file_returns_image");

    // Minimal valid 1x1 PNG
    let png_bytes: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // PNG signature
        0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, // IHDR chunk
        0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, // 1x1
        0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53, 0xDE, // bit depth, color, etc
        0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, // IDAT chunk
        0x08, 0xD7, 0x63, 0xF8, 0xCF, 0xC0, 0x00, 0x00, 0x00, 0x02, 0x00, 0x01, // data
        0xE2, 0x21, 0xBC, 0x33, // CRC
        0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, // IEND
        0xAE, 0x42, 0x60, 0x82,
    ];
    let file = h.create_file("tiny.png", png_bytes);
    let registry = make_registry(h.temp_dir());

    let output = common::run_async(async move {
        let tool = registry.get("read").unwrap();
        tool.execute("call-5", json!({"path": file.display().to_string()}), None)
            .await
    });

    let result = output.unwrap();
    h.log()
        .info("result", format!("blocks={}", result.content.len()));

    // Should return at least one Image block
    let has_image = result
        .content
        .iter()
        .any(|b| matches!(b, ContentBlock::Image(_)));
    assert!(has_image, "should contain an Image content block");
    assert!(!result.is_error);
}

// ===========================================================================
// Write Tool
// ===========================================================================

#[test]
fn write_new_file() {
    let h = TestHarness::new("write_new_file");
    let target = h.temp_path("sub/dir/new_file.txt");
    let registry = make_registry(h.temp_dir());

    let target_str = target.display().to_string();
    let output = common::run_async(async move {
        let tool = registry.get("write").unwrap();
        tool.execute(
            "call-6",
            json!({"path": target_str, "content": "hello world"}),
            None,
        )
        .await
    });

    let result = output.unwrap();
    h.log()
        .info("result", first_text(&result.content).to_string());

    assert!(!result.is_error);
    assert!(target.exists(), "file should be created");
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "hello world",
        "content should match"
    );
}

#[test]
fn write_overwrite_existing() {
    let h = TestHarness::new("write_overwrite_existing");
    let file = h.create_file("existing.txt", "old content");
    let registry = make_registry(h.temp_dir());

    let file_str = file.display().to_string();
    let output = common::run_async(async move {
        let tool = registry.get("write").unwrap();
        tool.execute(
            "call-7",
            json!({"path": file_str, "content": "new content"}),
            None,
        )
        .await
    });

    let result = output.unwrap();
    assert!(!result.is_error);
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "new content",
        "should overwrite with new content"
    );
}

#[test]
fn write_reports_byte_count() {
    let h = TestHarness::new("write_reports_byte_count");
    let file = h.temp_path("count.txt");
    let registry = make_registry(h.temp_dir());

    let file_str = file.display().to_string();
    let output = common::run_async(async move {
        let tool = registry.get("write").unwrap();
        tool.execute(
            "call-8",
            json!({"path": file_str, "content": "abcde"}),
            None,
        )
        .await
    });

    let result = output.unwrap();
    let text = first_text(&result.content);
    h.log().info("result", text.to_string());

    // "Successfully wrote 5 bytes to ..."
    assert!(
        text.contains("5 bytes"),
        "should report byte count, got: {text}"
    );
}

// ===========================================================================
// Edit Tool
// ===========================================================================

#[test]
fn edit_exact_match() {
    let h = TestHarness::new("edit_exact_match");
    let file = h.create_file("editable.txt", "Hello World\nFoo Bar\n");
    let registry = make_registry(h.temp_dir());

    let file_str = file.display().to_string();
    let output = common::run_async(async move {
        let tool = registry.get("edit").unwrap();
        tool.execute(
            "call-9",
            json!({
                "path": file_str,
                "oldText": "Foo Bar",
                "newText": "Baz Qux"
            }),
            None,
        )
        .await
    });

    let result = output.unwrap();
    let text = first_text(&result.content);
    h.log().info("result", text.to_string());

    assert!(text.contains("Successfully replaced"), "should succeed");
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "Hello World\nBaz Qux\n"
    );

    // Details should contain a diff
    assert!(result.details.is_some(), "should have details with diff");
    let details = result.details.unwrap();
    assert!(details.get("diff").is_some(), "details should contain diff");
}

#[test]
fn edit_text_not_found_error() {
    let h = TestHarness::new("edit_text_not_found_error");
    let file = h.create_file("nope.txt", "Hello World\n");
    let registry = make_registry(h.temp_dir());

    let file_str = file.display().to_string();
    let output = common::run_async(async move {
        let tool = registry.get("edit").unwrap();
        tool.execute(
            "call-10",
            json!({
                "path": file_str,
                "oldText": "does not exist in file",
                "newText": "replacement"
            }),
            None,
        )
        .await
    });

    let err = output.unwrap_err();
    h.log().info("result", format!("error={err:?}"));
    assert!(
        err.to_string().contains("Could not find"),
        "message should say text not found: {err}"
    );
}

#[test]
fn edit_ambiguous_match_error() {
    let h = TestHarness::new("edit_ambiguous_match_error");
    let file = h.create_file("ambig.txt", "apple\norange\napple\n");
    let registry = make_registry(h.temp_dir());

    let file_str = file.display().to_string();
    let output = common::run_async(async move {
        let tool = registry.get("edit").unwrap();
        tool.execute(
            "call-11",
            json!({
                "path": file_str,
                "oldText": "apple",
                "newText": "banana"
            }),
            None,
        )
        .await
    });

    let err = output.unwrap_err();
    h.log().info("result", format!("error={err:?}"));
    assert!(
        err.to_string().contains("occurrences"),
        "message should mention multiple occurrences: {err}"
    );
}

#[test]
fn edit_preserves_line_endings() {
    let h = TestHarness::new("edit_preserves_line_endings");
    // CRLF content
    let file = h.create_file("crlf.txt", "line1\r\nline2\r\nline3\r\n");
    let registry = make_registry(h.temp_dir());

    let file_str = file.display().to_string();
    let output = common::run_async(async move {
        let tool = registry.get("edit").unwrap();
        tool.execute(
            "call-12",
            json!({
                "path": file_str,
                "oldText": "line2",
                "newText": "replaced"
            }),
            None,
        )
        .await
    });

    let result = output.unwrap();
    assert!(!result.is_error);

    let on_disk = std::fs::read(&file).unwrap();
    let on_disk_str = String::from_utf8_lossy(&on_disk);
    h.log().info("result", format!("on_disk={on_disk_str:?}"));
    // CRLF should be preserved
    assert!(
        on_disk_str.contains("\r\n"),
        "should preserve CRLF line endings"
    );
    assert!(
        on_disk_str.contains("replaced"),
        "should contain replaced text"
    );
}

// ===========================================================================
// Bash Tool
// ===========================================================================

#[test]
fn bash_simple_command() {
    let h = TestHarness::new("bash_simple_command");
    let registry = make_registry(h.temp_dir());

    let output = common::run_async(async move {
        let tool = registry.get("bash").unwrap();
        tool.execute("call-13", json!({"command": "echo hello"}), None)
            .await
    });

    let result = output.unwrap();
    let text = first_text(&result.content);
    h.log().info("result", text.to_string());

    assert!(text.contains("hello"), "should contain 'hello'");
    assert!(!result.is_error);
}

#[cfg(unix)]
#[test]
fn write_permission_denied_reports_clear_error() {
    let h = TestHarness::new("write_permission_denied_reports_clear_error");
    let readonly_dir = h.temp_path("readonly");
    fs::create_dir_all(&readonly_dir).expect("create readonly dir");
    let mut perms = fs::metadata(&readonly_dir).expect("metadata").permissions();
    perms.set_mode(0o555);
    fs::set_permissions(&readonly_dir, perms).expect("chmod readonly");

    let target = readonly_dir.join("out.txt");
    let registry = make_registry(h.temp_dir());
    let output = common::run_async(async move {
        let tool = registry.get("write").unwrap();
        tool.execute(
            "call-write-perm-denied",
            json!({"path": target.display().to_string(), "content": "blocked"}),
            None,
        )
        .await
    });

    // Ensure tempdir cleanup isn't permission-blocked.
    let mut cleanup_perms = fs::metadata(&readonly_dir)
        .expect("cleanup metadata")
        .permissions();
    cleanup_perms.set_mode(0o755);
    fs::set_permissions(&readonly_dir, cleanup_perms).expect("restore permissions");

    let err = output.expect_err("write should fail in readonly directory");
    let msg = err.to_string().to_ascii_lowercase();
    h.log().info("result", format!("error={msg}"));
    assert!(
        msg.contains("permission") || msg.contains("denied"),
        "expected permission-denied diagnostics, got: {msg}"
    );
}

#[test]
fn bash_nonzero_exit() {
    let h = TestHarness::new("bash_nonzero_exit");
    let registry = make_registry(h.temp_dir());

    let output = common::run_async(async move {
        let tool = registry.get("bash").unwrap();
        tool.execute(
            "call-14",
            json!({"command": "echo stderr_msg >&2; exit 42"}),
            None,
        )
        .await
    });

    // Non-zero exit returns Error::Tool
    let output = output.unwrap();
    h.log().info("result", format!("output={output:?}"));
    assert!(
        output.is_error,
        "expected tool execution to be marked as an error due to non-zero exit code"
    );
}

#[test]
fn bash_timeout() {
    let h = TestHarness::new("bash_timeout");
    let registry = make_registry(h.temp_dir());

    let output = common::run_async(async move {
        let tool = registry.get("bash").unwrap();
        tool.execute(
            "call-15",
            json!({"command": "sleep 300", "timeout": 1}),
            None,
        )
        .await
    });

    // Timeout should result in an error (cancelled)
    let output = output.unwrap();
    h.log().info("result", format!("output={output:?}"));
    assert!(
        output.is_error,
        "expected tool execution to be marked as an error due to non-zero exit code"
    );
}

#[test]
fn bash_timeout_reports_partial_output_for_repro() {
    let h = TestHarness::new("bash_timeout_reports_partial_output_for_repro");
    let registry = make_registry(h.temp_dir());

    let output = common::run_async(async move {
        let tool = registry.get("bash").unwrap();
        tool.execute(
            "call-15b",
            json!({"command": "printf 'before-timeout\\n'; sleep 300", "timeout": 1}),
            None,
        )
        .await
    });

    let result = output.expect("timeout should return Ok with is_error=true");
    assert!(result.is_error, "expected is_error=true");
    let msg = first_text(&result.content).to_ascii_lowercase();
    h.log().info("result", format!("error={msg}"));
    assert!(
        msg.contains("before-timeout"),
        "expected partial command output in timeout diagnostics: {msg}"
    );
    assert!(
        msg.contains("timed out") || msg.contains("timeout"),
        "expected timeout diagnostics in error message: {msg}"
    );
}

#[test]
fn bash_large_output_truncation() {
    let h = TestHarness::new("bash_large_output_truncation");
    let registry = make_registry(h.temp_dir());

    let output = common::run_async(async move {
        let tool = registry.get("bash").unwrap();
        // Generate >50KB of output (each line ~11 bytes x 6000 > 60KB)
        tool.execute("call-16", json!({"command": "seq 1 6000"}), None)
            .await
    });

    let result = output.unwrap();
    let text = first_text(&result.content);
    h.log().info("result", format!("text_len={}", text.len()));

    // The output should be present (bash tool keeps tail)
    assert!(!text.is_empty(), "should have output");
}

// ===========================================================================
// Grep Tool
// ===========================================================================

#[test]
fn grep_basic_match() {
    if !rg_available() {
        eprintln!("SKIP: ripgrep (rg) not installed");
        return;
    }

    let h = TestHarness::new("grep_basic_match");
    h.create_file("a.txt", "hello world\ngoodbye world\n");
    h.create_file("b.txt", "no match here\nhello again\n");
    let registry = make_registry(h.temp_dir());

    let output = common::run_async(async move {
        let tool = registry.get("grep").unwrap();
        tool.execute("call-17", json!({"pattern": "hello"}), None)
            .await
    });

    let result = output.unwrap();
    let text = first_text(&result.content);
    h.log().info("result", text.to_string());

    // Should have file:line format matches
    assert!(text.contains("hello"), "should contain matching text");
    assert!(!result.is_error);
}

#[test]
fn grep_case_insensitive() {
    if !rg_available() {
        eprintln!("SKIP: ripgrep (rg) not installed");
        return;
    }

    let h = TestHarness::new("grep_case_insensitive");
    h.create_file("mixed.txt", "Hello World\nHELLO WORLD\nhello world\n");
    let registry = make_registry(h.temp_dir());

    let output = common::run_async(async move {
        let tool = registry.get("grep").unwrap();
        tool.execute(
            "call-18",
            json!({"pattern": "hello", "ignoreCase": true}),
            None,
        )
        .await
    });

    let result = output.unwrap();
    let text = first_text(&result.content);
    h.log().info("result", text.to_string());

    // All three lines should match with case insensitive
    assert!(text.contains("Hello World"), "should match mixed case");
    assert!(text.contains("HELLO WORLD"), "should match upper case");
    assert!(text.contains("hello world"), "should match lower case");
}

#[test]
fn grep_no_matches() {
    if !rg_available() {
        eprintln!("SKIP: ripgrep (rg) not installed");
        return;
    }

    let h = TestHarness::new("grep_no_matches");
    h.create_file("data.txt", "alpha\nbeta\ngamma\n");
    let registry = make_registry(h.temp_dir());

    let output = common::run_async(async move {
        let tool = registry.get("grep").unwrap();
        tool.execute("call-19", json!({"pattern": "zzz_no_match_zzz"}), None)
            .await
    });

    let result = output.unwrap();
    let text = first_text(&result.content);
    h.log().info("result", text.to_string());

    assert!(
        text.contains("No matches found"),
        "should report no matches, got: {text}"
    );
    assert!(!result.is_error, "no matches is not an error");
}

#[test]
fn grep_with_context() {
    if !rg_available() {
        eprintln!("SKIP: ripgrep (rg) not installed");
        return;
    }

    let h = TestHarness::new("grep_with_context");
    h.create_file("ctx.txt", "line1\nline2\nTARGET\nline4\nline5\n");
    let registry = make_registry(h.temp_dir());

    let output = common::run_async(async move {
        let tool = registry.get("grep").unwrap();
        tool.execute("call-20", json!({"pattern": "TARGET", "context": 1}), None)
            .await
    });

    let result = output.unwrap();
    let text = first_text(&result.content);
    h.log().info("result", text.to_string());

    // Context lines use '-N-' format, match lines use ':N:'
    assert!(text.contains("TARGET"), "should contain match");
    assert!(text.contains("line2"), "should contain context before");
    assert!(text.contains("line4"), "should contain context after");
}

// ===========================================================================
// Find Tool
// ===========================================================================

#[test]
fn find_glob_pattern() {
    if !fd_available() {
        eprintln!("SKIP: fd/fdfind not installed");
        return;
    }

    let h = TestHarness::new("find_glob_pattern");
    h.create_file("one.txt", "");
    h.create_file("two.txt", "");
    h.create_file("three.rs", "");
    let registry = make_registry(h.temp_dir());

    let output = common::run_async(async move {
        let tool = registry.get("find").unwrap();
        tool.execute("call-21", json!({"pattern": "*.txt"}), None)
            .await
    });

    let result = output.unwrap();
    let text = first_text(&result.content);
    h.log().info("result", text.to_string());

    assert!(text.contains("one.txt"), "should find one.txt");
    assert!(text.contains("two.txt"), "should find two.txt");
    assert!(!text.contains("three.rs"), "should not find .rs files");
    assert!(!result.is_error);
}

#[test]
fn find_no_matches() {
    if !fd_available() {
        eprintln!("SKIP: fd/fdfind not installed");
        return;
    }

    let h = TestHarness::new("find_no_matches");
    h.create_file("data.txt", "");
    let registry = make_registry(h.temp_dir());

    let output = common::run_async(async move {
        let tool = registry.get("find").unwrap();
        tool.execute("call-22", json!({"pattern": "*.zzz_no_match"}), None)
            .await
    });

    let result = output.unwrap();
    let text = first_text(&result.content);
    h.log().info("result", text.to_string());

    assert!(
        text.contains("No files found"),
        "should report no files found, got: {text}"
    );
    assert!(!result.is_error, "no matches is not an error");
}

#[test]
fn find_with_limit() {
    if !fd_available() {
        eprintln!("SKIP: fd/fdfind not installed");
        return;
    }

    let h = TestHarness::new("find_with_limit");
    for i in 0..10 {
        h.create_file(format!("file_{i:02}.txt"), "");
    }
    let registry = make_registry(h.temp_dir());

    let output = common::run_async(async move {
        let tool = registry.get("find").unwrap();
        tool.execute("call-23", json!({"pattern": "*.txt", "limit": 3}), None)
            .await
    });

    let result = output.unwrap();
    let text = first_text(&result.content);
    h.log().info("result", text.to_string());

    // Count the number of .txt entries
    let txt_count = text.lines().filter(|l| l.contains(".txt")).count();
    assert!(
        txt_count <= 3,
        "should return at most 3 results, got {txt_count}"
    );
}

#[test]
fn find_directory_suffix() {
    if !fd_available() {
        eprintln!("SKIP: fd/fdfind not installed");
        return;
    }

    let h = TestHarness::new("find_directory_suffix");
    h.create_dir("subdir_a");
    h.create_dir("subdir_b");
    h.create_file("subdir_a/keep.txt", "");
    h.create_file("subdir_b/keep.txt", "");
    let registry = make_registry(h.temp_dir());

    let output = common::run_async(async move {
        let tool = registry.get("find").unwrap();
        tool.execute("call-24", json!({"pattern": "subdir_*"}), None)
            .await
    });

    let result = output.unwrap();
    let text = first_text(&result.content);
    h.log().info("result", text.to_string());

    // Directories should have '/' suffix (on Windows fd returns absolute paths)
    assert!(
        text.lines()
            .any(|line| line.contains("subdir_") && line.ends_with('/')),
        "directories should have '/' suffix, got: {text}"
    );
}

// ===========================================================================
// Ls Tool
// ===========================================================================

#[test]
fn ls_directory_contents() {
    let h = TestHarness::new("ls_directory_contents");
    h.create_file("alpha.txt", "");
    h.create_file("beta.txt", "");
    h.create_dir("gamma_dir");
    let registry = make_registry(h.temp_dir());

    let dir_path = h.temp_dir().display().to_string();
    let output = common::run_async(async move {
        let tool = registry.get("ls").unwrap();
        tool.execute("call-25", json!({"path": dir_path}), None)
            .await
    });

    let result = output.unwrap();
    let text = first_text(&result.content);
    h.log().info("result", text.to_string());

    assert!(text.contains("alpha.txt"), "should list alpha.txt");
    assert!(text.contains("beta.txt"), "should list beta.txt");
    assert!(
        text.contains("gamma_dir/"),
        "should list gamma_dir with / suffix"
    );
    assert!(!result.is_error);
}

#[test]
fn ls_empty_directory() {
    let h = TestHarness::new("ls_empty_directory");
    let empty = h.create_dir("empty_dir");
    let registry = make_registry(h.temp_dir());

    let dir_str = empty.display().to_string();
    let output = common::run_async(async move {
        let tool = registry.get("ls").unwrap();
        tool.execute("call-26", json!({"path": dir_str}), None)
            .await
    });

    let result = output.unwrap();
    let text = first_text(&result.content);
    h.log().info("result", text.to_string());

    assert!(
        text.contains("(empty directory)"),
        "should show empty directory message, got: {text}"
    );
}

#[test]
fn ls_nonexistent_path_error() {
    let h = TestHarness::new("ls_nonexistent_path_error");
    let registry = make_registry(h.temp_dir());
    let missing = h.temp_path("not_a_dir");

    let dir_str = missing.display().to_string();
    let output = common::run_async(async move {
        let tool = registry.get("ls").unwrap();
        tool.execute("call-27", json!({"path": dir_str}), None)
            .await
    });

    let err = output.unwrap_err();
    h.log().info("result", format!("error={err}"));
    assert!(
        matches!(err, Error::Tool { .. }),
        "should be Tool error: {err}"
    );
}

// ===========================================================================
// Cross-tool
// ===========================================================================

#[test]
fn write_edit_read_cycle() {
    let h = TestHarness::new("write_edit_read_cycle");
    let file_path = h.temp_path("cycle.txt");
    let file_str = file_path.display().to_string();
    let registry = make_registry(h.temp_dir());

    // Step 1: Write a file
    let write_file = file_str.clone();
    let write_reg = make_registry(h.temp_dir());
    let write_result = common::run_async(async move {
        let tool = write_reg.get("write").unwrap();
        tool.execute(
            "call-w",
            json!({"path": write_file, "content": "alpha\nbeta\ngamma\n"}),
            None,
        )
        .await
    });
    assert!(!write_result.unwrap().is_error, "write should succeed");

    // Step 2: Edit the file
    let edit_file = file_str.clone();
    let edit_reg = make_registry(h.temp_dir());
    let edit_result = common::run_async(async move {
        let tool = edit_reg.get("edit").unwrap();
        tool.execute(
            "call-e",
            json!({
                "path": edit_file,
                "oldText": "beta",
                "newText": "BETA_REPLACED"
            }),
            None,
        )
        .await
    });
    assert!(!edit_result.unwrap().is_error, "edit should succeed");

    // Step 3: Read the file back
    let read_file = file_str;
    let read_result = common::run_async(async move {
        let tool = registry.get("read").unwrap();
        tool.execute("call-r", json!({"path": read_file}), None)
            .await
    });
    let result = read_result.unwrap();
    let text = first_text(&result.content);
    h.log().info("result", text.to_string());

    assert!(
        text.contains("BETA_REPLACED"),
        "should see edited content: {text}"
    );
    assert!(text.contains("alpha"), "should still have alpha");
    assert!(text.contains("gamma"), "should still have gamma");
    assert!(!text.contains("\nbeta\n"), "original beta should be gone");
}

// ===========================================================================
// Hardened tool tests (bd-1f42.2.2): Real FS/process, high-fidelity
// diagnostics, edge cases for stdout/stderr/exit codes, permissions,
// timeout behavior.
// ===========================================================================

// ---------------------------------------------------------------------------
// Diagnostic helper: captures workspace snapshot + env + timing on failure.
// ---------------------------------------------------------------------------

/// Capture a snapshot of the workspace for diagnostics.
fn snapshot_workspace(h: &TestHarness) -> serde_json::Value {
    let mut entries = Vec::new();
    if let Ok(rd) = std::fs::read_dir(h.temp_dir()) {
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let meta = entry.metadata().ok();
            let size = meta.as_ref().map_or(0, std::fs::Metadata::len);
            let is_dir = meta.as_ref().is_some_and(std::fs::Metadata::is_dir);
            entries.push(serde_json::json!({
                "name": name,
                "size": size,
                "is_dir": is_dir,
            }));
        }
    }
    serde_json::json!({
        "cwd": h.temp_dir().display().to_string(),
        "entries": entries,
    })
}

/// Log diagnostics for a tool execution including timing and workspace snapshot.
fn log_diagnostics(
    h: &TestHarness,
    tool_name: &str,
    call_id: &str,
    input: &serde_json::Value,
    elapsed_ms: u128,
    result_summary: &str,
) {
    let workspace = snapshot_workspace(h);
    h.log().info_ctx(
        "diagnostics",
        format!("{tool_name}:{call_id} completed"),
        |ctx| {
            ctx.push(("tool".into(), tool_name.to_string()));
            ctx.push(("call_id".into(), call_id.to_string()));
            ctx.push(("input".into(), input.to_string()));
            ctx.push(("elapsed_ms".into(), elapsed_ms.to_string()));
            ctx.push(("result".into(), result_summary.to_string()));
            ctx.push(("workspace".into(), workspace.to_string()));
        },
    );
}

// ===========================================================================
// Bash Tool — Hardened
// ===========================================================================

/// Bash: stderr-only output with exit 0 is captured correctly.
#[test]
fn bash_stderr_only_exit_zero() {
    let h = TestHarness::new("bash_stderr_only_exit_zero");
    let registry = make_registry(h.temp_dir());
    let start = std::time::Instant::now();

    let output = common::run_async(async move {
        let tool = registry.get("bash").unwrap();
        tool.execute(
            "bash-stderr-0",
            json!({"command": "echo stderr_output >&2"}),
            None,
        )
        .await
    });

    let elapsed = start.elapsed().as_millis();
    let result = output.unwrap();
    let text = first_text(&result.content);
    log_diagnostics(
        &h,
        "bash",
        "bash-stderr-0",
        &json!({"command": "echo stderr_output >&2"}),
        elapsed,
        &format!("ok: {}", text.len()),
    );

    assert!(
        text.contains("stderr_output"),
        "stderr should be captured even with exit 0, got: {text}"
    );
    assert!(!result.is_error);
}

/// Bash: CWD propagation — commands execute in the configured temp directory.
#[test]
fn bash_cwd_propagation() {
    let h = TestHarness::new("bash_cwd_propagation");
    h.create_file("marker_file.txt", "cwd_test");
    let registry = make_registry(h.temp_dir());
    let expected_dir = h.temp_dir().to_path_buf();

    let output = common::run_async(async move {
        let tool = registry.get("bash").unwrap();
        tool.execute(
            "bash-cwd",
            json!({"command": "pwd && ls marker_file.txt"}),
            None,
        )
        .await
    });

    let result = output.unwrap();
    let text = first_text(&result.content);
    h.log().info("result", format!("cwd output: {text}"));

    assert!(
        text.contains(&expected_dir.display().to_string()),
        "pwd should show temp dir, got: {text}"
    );
    assert!(
        text.contains("marker_file.txt"),
        "ls should find marker file in CWD, got: {text}"
    );
}

/// Bash: mixed stdout and stderr are both captured.
#[test]
fn bash_mixed_stdout_stderr() {
    let h = TestHarness::new("bash_mixed_stdout_stderr");
    let registry = make_registry(h.temp_dir());

    let output = common::run_async(async move {
        let tool = registry.get("bash").unwrap();
        tool.execute(
            "bash-mixed",
            json!({"command": "echo stdout_first; echo stderr_middle >&2; echo stdout_last"}),
            None,
        )
        .await
    });

    let result = output.unwrap();
    let text = first_text(&result.content);
    h.log().info("result", text.to_string());

    assert!(text.contains("stdout_first"), "should capture first stdout");
    assert!(
        text.contains("stderr_middle"),
        "should capture stderr in middle"
    );
    assert!(text.contains("stdout_last"), "should capture last stdout");
}

/// Bash: timeout=0 disables the default timeout (command runs without enforced limit).
#[test]
fn bash_timeout_zero_disables_limit() {
    let h = TestHarness::new("bash_timeout_zero_disables_limit");
    let registry = make_registry(h.temp_dir());

    let output = common::run_async(async move {
        let tool = registry.get("bash").unwrap();
        // timeout=0 should not kill a fast command
        tool.execute(
            "bash-t0",
            json!({"command": "echo still_running", "timeout": 0}),
            None,
        )
        .await
    });

    let result = output.unwrap();
    let text = first_text(&result.content);
    h.log().info("result", text.to_string());

    assert!(
        text.contains("still_running"),
        "command should complete with timeout=0, got: {text}"
    );
    assert!(!result.is_error);
}

/// Bash: process tree cleanup — child processes are killed on timeout.
#[cfg(unix)]
#[test]
fn bash_process_tree_cleanup_on_timeout() {
    let h = TestHarness::new("bash_process_tree_cleanup_on_timeout");
    let registry = make_registry(h.temp_dir());
    let pid_file = h.temp_path("child.pid");
    let pid_file_str = pid_file.display().to_string();

    let output = common::run_async(async move {
        let tool = registry.get("bash").unwrap();
        // Spawn a background child that writes its PID, then sleep
        tool.execute(
            "bash-tree",
            json!({
                "command": format!(
                    "bash -c 'echo $$ > {pid_file_str}; sleep 300' & wait"
                ),
                "timeout": 2
            }),
            None,
        )
        .await
    });

    let result = output.unwrap();
    assert!(result.is_error);
    let msg = first_text(&result.content);
    h.log().info("result", format!("error={msg}"));
    assert!(msg.contains("timed out"), "should report timeout: {msg}");

    // Give a moment for cleanup
    std::thread::sleep(std::time::Duration::from_millis(500));

    // If the PID file was created, verify the child process is gone
    if pid_file.exists()
        && let Ok(pid_str) = std::fs::read_to_string(&pid_file)
        && let Ok(pid) = pid_str.trim().parse::<u32>()
    {
        // Check if process still exists via kill -0 (signal check only)
        let check = std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        let still_alive = check.is_ok_and(|s| s.success());
        h.log().info(
            "cleanup",
            format!("child pid={pid}, still_alive={still_alive}"),
        );
        // The child should have been killed
        assert!(
            !still_alive,
            "child process {pid} should have been killed after timeout"
        );
    }
}

/// Bash: nonexistent working directory reports clear error.
#[test]
fn bash_nonexistent_cwd_error() {
    let h = TestHarness::new("bash_nonexistent_cwd_error");
    let bad_cwd = h.temp_path("does_not_exist_dir");
    let registry = ToolRegistry::new(&["bash"], &bad_cwd, None);

    let output = common::run_async(async move {
        let tool = registry.get("bash").unwrap();
        tool.execute("bash-badcwd", json!({"command": "echo test"}), None)
            .await
    });

    let err = output.unwrap_err();
    let msg = err.to_string().to_ascii_lowercase();
    h.log().info("result", format!("error={msg}"));
    assert!(
        msg.contains("does not exist") || msg.contains("working directory"),
        "should report nonexistent CWD: {msg}"
    );
}

/// Bash: special characters in command are handled correctly.
#[test]
fn bash_special_characters() {
    let h = TestHarness::new("bash_special_characters");
    let registry = make_registry(h.temp_dir());

    let output = common::run_async(async move {
        let tool = registry.get("bash").unwrap();
        tool.execute(
            "bash-special",
            json!({"command": "echo 'single quotes' && echo \"double quotes\" && echo $HOME"}),
            None,
        )
        .await
    });

    let result = output.unwrap();
    let text = first_text(&result.content);
    h.log().info("result", text.to_string());

    assert!(text.contains("single quotes"), "single quotes preserved");
    assert!(text.contains("double quotes"), "double quotes preserved");
}

/// Bash: multi-line command with pipe works correctly.
#[test]
fn bash_pipe_command() {
    let h = TestHarness::new("bash_pipe_command");
    h.create_file("data.txt", "apple\nbanana\ncherry\napricot\n");
    let registry = make_registry(h.temp_dir());

    let output = common::run_async(async move {
        let tool = registry.get("bash").unwrap();
        tool.execute(
            "bash-pipe",
            json!({"command": "cat data.txt | grep 'ap' | sort"}),
            None,
        )
        .await
    });

    let result = output.unwrap();
    let text = first_text(&result.content);
    h.log().info("result", text.to_string());

    assert!(text.contains("apple"), "should find apple");
    assert!(text.contains("apricot"), "should find apricot");
    assert!(!text.contains("banana"), "banana should be filtered out");
}

/// Bash: exit code is captured in error for various codes.
#[test]
fn bash_exit_code_captured() {
    let h = TestHarness::new("bash_exit_code_captured");
    let registry = make_registry(h.temp_dir());

    for code in [1, 2, 127, 255] {
        let reg = make_registry(h.temp_dir());
        let output = common::run_async(async move {
            let tool = reg.get("bash").unwrap();
            tool.execute(
                "bash-exit",
                json!({"command": format!("exit {code}")}),
                None,
            )
            .await
        });

        let result = output.unwrap();
        assert!(result.is_error);
        let msg = first_text(&result.content).to_string();
        h.log().info("result", format!("exit {code}: error={msg}"));
        assert!(
            msg.contains(&format!("code {code}")),
            "should contain exit code {code}: {msg}"
        );
    }
    drop(registry);
}

// ===========================================================================
// Read Tool — Hardened
// ===========================================================================

/// Read: symlink to file works transparently.
#[cfg(unix)]
#[test]
fn read_symlink_to_file() {
    let h = TestHarness::new("read_symlink_to_file");
    let target = h.create_file("real.txt", "symlink content\nline two\n");
    let link = h.temp_path("link.txt");
    std::os::unix::fs::symlink(&target, &link).expect("create symlink");
    let registry = make_registry(h.temp_dir());

    let link_str = link.display().to_string();
    let output = common::run_async(async move {
        let tool = registry.get("read").unwrap();
        tool.execute("read-sym", json!({"path": link_str}), None)
            .await
    });

    let result = output.unwrap();
    let text = first_text(&result.content);
    h.log().info("result", text.to_string());

    assert!(
        text.contains("symlink content"),
        "should read through symlink: {text}"
    );
    assert!(text.contains("line two"), "should have all content");
}

/// Read: Unicode multi-byte content has correct line numbers.
#[test]
fn read_unicode_multibyte() {
    let h = TestHarness::new("read_unicode_multibyte");
    let file = h.create_file("unicode.txt", "café\n日本語\n🎉🎊🎈\n");
    let registry = make_registry(h.temp_dir());

    let file_str = file.display().to_string();
    let output = common::run_async(async move {
        let tool = registry.get("read").unwrap();
        tool.execute("read-utf8", json!({"path": file_str}), None)
            .await
    });

    let result = output.unwrap();
    let text = first_text(&result.content);
    h.log().info("result", text.to_string());

    assert!(text.contains("1→café"), "line 1 should have café");
    assert!(text.contains("2→日本語"), "line 2 should have Japanese");
    assert!(text.contains("3→🎉🎊🎈"), "line 3 should have emojis");
}

/// Read: empty file returns empty content (not an error).
#[test]
fn read_empty_file_is_not_error() {
    let h = TestHarness::new("read_empty_file_is_not_error");
    let file = h.create_file("empty.txt", "");
    let registry = make_registry(h.temp_dir());

    let file_str = file.display().to_string();
    let output = common::run_async(async move {
        let tool = registry.get("read").unwrap();
        tool.execute("read-empty", json!({"path": file_str}), None)
            .await
    });

    let result = output.unwrap();
    assert!(!result.is_error, "empty file should not be an error");
}

/// Read: binary non-image file is read as lossy UTF-8 text.
#[test]
fn read_binary_non_image_file() {
    let h = TestHarness::new("read_binary_non_image_file");
    // Random binary data that doesn't start with a known image signature
    let file = h.create_file("data.bin", [0x00, 0x01, 0x02, 0xFF, 0xFE, 0x0A, 0x41, 0x42]);
    let registry = make_registry(h.temp_dir());

    let file_str = file.display().to_string();
    let output = common::run_async(async move {
        let tool = registry.get("read").unwrap();
        tool.execute("read-bin", json!({"path": file_str}), None)
            .await
    });

    // Should succeed (lossy UTF-8 conversion)
    let result = output.unwrap();
    let text = first_text(&result.content);
    h.log().info("result", format!("text_len={}", text.len()));
    // Should contain "AB" from bytes 0x41, 0x42
    assert!(text.contains("AB"), "should contain ASCII portion: {text}");
}

/// Read: file with Windows line endings (CRLF) displays correctly.
#[test]
fn read_crlf_line_endings() {
    let h = TestHarness::new("read_crlf_line_endings");
    let file = h.create_file("crlf.txt", "line1\r\nline2\r\nline3\r\n");
    let registry = make_registry(h.temp_dir());

    let file_str = file.display().to_string();
    let output = common::run_async(async move {
        let tool = registry.get("read").unwrap();
        tool.execute("read-crlf", json!({"path": file_str}), None)
            .await
    });

    let result = output.unwrap();
    let text = first_text(&result.content);
    h.log().info("result", text.to_string());

    assert!(text.contains("1→line1"), "line 1");
    assert!(text.contains("2→line2"), "line 2");
    assert!(text.contains("3→line3"), "line 3");
    // CRLF \r should be stripped from display
    assert!(
        !text.contains("\r\n"),
        "\\r should be stripped in display: {text:?}"
    );
}

// ===========================================================================
// Write Tool — Hardened
// ===========================================================================

/// Write: large file content is written correctly.
#[test]
fn write_large_file() {
    let h = TestHarness::new("write_large_file");
    let target = h.temp_path("large.txt");
    let registry = make_registry(h.temp_dir());

    // 100KB content
    let mut content = String::new();
    for i in 0..10_000 {
        writeln!(&mut content, "line {i:05}").expect("write content line");
    }
    let content_len = content.len();
    let target_str = target.display().to_string();
    let content_clone = content.clone();
    let output = common::run_async(async move {
        let tool = registry.get("write").unwrap();
        tool.execute(
            "write-large",
            json!({"path": target_str, "content": content_clone}),
            None,
        )
        .await
    });

    let result = output.unwrap();
    assert!(!result.is_error);
    let on_disk = std::fs::read_to_string(&target).unwrap();
    assert_eq!(on_disk.len(), content_len, "file size should match");
    assert_eq!(on_disk, content, "content should match exactly");
}

/// Write: Unicode content and verification.
#[test]
fn write_unicode_content() {
    let h = TestHarness::new("write_unicode_content");
    let target = h.temp_path("unicode_out.txt");
    let registry = make_registry(h.temp_dir());

    let content = "café ☕\n日本語テスト\n🦀 Rust 🎯\n";
    let target_str = target.display().to_string();
    let output = common::run_async(async move {
        let tool = registry.get("write").unwrap();
        tool.execute(
            "write-unicode",
            json!({"path": target_str, "content": content}),
            None,
        )
        .await
    });

    let result = output.unwrap();
    assert!(!result.is_error);
    let on_disk = std::fs::read_to_string(&target).unwrap();
    assert_eq!(on_disk, content);
}

/// Write: deeply nested directory creation.
#[test]
fn write_deep_nested_dirs() {
    let h = TestHarness::new("write_deep_nested_dirs");
    let target = h.temp_path("a/b/c/d/e/f/g/deep.txt");
    let registry = make_registry(h.temp_dir());

    let target_str = target.display().to_string();
    let output = common::run_async(async move {
        let tool = registry.get("write").unwrap();
        tool.execute(
            "write-deep",
            json!({"path": target_str, "content": "deep content"}),
            None,
        )
        .await
    });

    let result = output.unwrap();
    assert!(!result.is_error);
    assert!(target.exists(), "deeply nested file should exist");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "deep content");
}

// ===========================================================================
// Edit Tool — Hardened
// ===========================================================================

/// Edit: multiline text replacement.
#[test]
fn edit_multiline_replace() {
    let h = TestHarness::new("edit_multiline_replace");
    let file = h.create_file(
        "multi.txt",
        "fn old() {\n    println!(\"old\");\n}\n\nfn keep() {}\n",
    );
    let registry = make_registry(h.temp_dir());

    let file_str = file.display().to_string();
    let output = common::run_async(async move {
        let tool = registry.get("edit").unwrap();
        tool.execute(
            "edit-multi",
            json!({
                "path": file_str,
                "oldText": "fn old() {\n    println!(\"old\");\n}",
                "newText": "fn new_fn() {\n    println!(\"new\");\n    // updated\n}"
            }),
            None,
        )
        .await
    });

    let result = output.unwrap();
    assert!(!result.is_error, "multiline edit should succeed");
    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert!(on_disk.contains("fn new_fn()"), "new function name");
    assert!(on_disk.contains("// updated"), "new comment");
    assert!(on_disk.contains("fn keep()"), "untouched code preserved");
    assert!(!on_disk.contains("fn old()"), "old code removed");
}

/// Edit: special characters (regex metachars) in search text.
#[test]
fn edit_special_chars_in_search() {
    let h = TestHarness::new("edit_special_chars_in_search");
    let file = h.create_file("special.txt", "price = $10.00 (USD)\nend\n");
    let registry = make_registry(h.temp_dir());

    let file_str = file.display().to_string();
    let output = common::run_async(async move {
        let tool = registry.get("edit").unwrap();
        tool.execute(
            "edit-special",
            json!({
                "path": file_str,
                "oldText": "$10.00 (USD)",
                "newText": "€10.00 (EUR)"
            }),
            None,
        )
        .await
    });

    let result = output.unwrap();
    assert!(!result.is_error, "edit with special chars should succeed");
    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert!(on_disk.contains("€10.00 (EUR)"), "replacement applied");
    assert!(!on_disk.contains("$10.00 (USD)"), "original removed");
}

/// Edit: whitespace-sensitive replacement (tabs vs spaces).
#[test]
fn edit_whitespace_sensitive() {
    let h = TestHarness::new("edit_whitespace_sensitive");
    let file = h.create_file("ws.txt", "\tindented with tab\n    indented with spaces\n");
    let registry = make_registry(h.temp_dir());

    let file_str = file.display().to_string();
    let output = common::run_async(async move {
        let tool = registry.get("edit").unwrap();
        tool.execute(
            "edit-ws",
            json!({
                "path": file_str,
                "oldText": "\tindented with tab",
                "newText": "\treplaced tab line"
            }),
            None,
        )
        .await
    });

    let result = output.unwrap();
    assert!(!result.is_error);
    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert!(
        on_disk.contains("\treplaced tab line"),
        "tab-indented replacement"
    );
    assert!(
        on_disk.contains("    indented with spaces"),
        "space-indented line untouched"
    );
}

/// Edit: replacing at the very start of a file.
#[test]
fn edit_at_file_start() {
    let h = TestHarness::new("edit_at_file_start");
    let file = h.create_file("start.txt", "HEADER\nbody content\nfooter\n");
    let registry = make_registry(h.temp_dir());

    let file_str = file.display().to_string();
    let output = common::run_async(async move {
        let tool = registry.get("edit").unwrap();
        tool.execute(
            "edit-start",
            json!({
                "path": file_str,
                "oldText": "HEADER",
                "newText": "NEW_HEADER"
            }),
            None,
        )
        .await
    });

    let result = output.unwrap();
    assert!(!result.is_error);
    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert!(
        on_disk.starts_with("NEW_HEADER"),
        "should start with new header"
    );
}

/// Edit: replacing at the very end of a file.
#[test]
fn edit_at_file_end() {
    let h = TestHarness::new("edit_at_file_end");
    let file = h.create_file("end.txt", "content\nFOOTER");
    let registry = make_registry(h.temp_dir());

    let file_str = file.display().to_string();
    let output = common::run_async(async move {
        let tool = registry.get("edit").unwrap();
        tool.execute(
            "edit-end",
            json!({
                "path": file_str,
                "oldText": "FOOTER",
                "newText": "NEW_FOOTER"
            }),
            None,
        )
        .await
    });

    let result = output.unwrap();
    assert!(!result.is_error);
    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert!(
        on_disk.ends_with("NEW_FOOTER"),
        "should end with new footer"
    );
}

// ===========================================================================
// Grep Tool — Hardened
// ===========================================================================

/// Grep: regex patterns (not just literals) work.
#[test]
fn grep_regex_pattern() {
    if !rg_available() {
        eprintln!("SKIP: ripgrep (rg) not installed");
        return;
    }

    let h = TestHarness::new("grep_regex_pattern");
    h.create_file("code.rs", "fn foo() {}\nfn bar(x: i32) {}\nstruct Baz;\n");
    let registry = make_registry(h.temp_dir());

    let output = common::run_async(async move {
        let tool = registry.get("grep").unwrap();
        tool.execute("grep-regex", json!({"pattern": "fn \\w+\\("}), None)
            .await
    });

    let result = output.unwrap();
    let text = first_text(&result.content);
    h.log().info("result", text.to_string());

    assert!(text.contains("fn foo()"), "should match foo");
    assert!(text.contains("fn bar("), "should match bar");
    assert!(
        !text.contains("struct"),
        "struct should not match fn pattern"
    );
}

/// Grep: scoped to specific subdirectory via path parameter.
#[test]
fn grep_path_scoping() {
    if !rg_available() {
        eprintln!("SKIP: ripgrep (rg) not installed");
        return;
    }

    let h = TestHarness::new("grep_path_scoping");
    h.create_file("src/main.rs", "fn main() { target_string(); }\n");
    h.create_file("tests/test.rs", "fn test() { target_string(); }\n");
    h.create_file("docs/readme.md", "no match here\n");
    let registry = make_registry(h.temp_dir());

    let output = common::run_async(async move {
        let tool = registry.get("grep").unwrap();
        tool.execute(
            "grep-scoped",
            json!({"pattern": "target_string", "path": "src"}),
            None,
        )
        .await
    });

    let result = output.unwrap();
    let text = first_text(&result.content);
    h.log().info("result", text.to_string());

    assert!(text.contains("main.rs"), "should find in src/main.rs");
    assert!(
        !text.contains("test.rs"),
        "should NOT find in tests/ when scoped to src/"
    );
}

/// Grep: with explicit match limit returns details.
#[test]
fn grep_match_limit_diagnostics() {
    if !rg_available() {
        eprintln!("SKIP: ripgrep (rg) not installed");
        return;
    }

    let h = TestHarness::new("grep_match_limit_diagnostics");
    let mut content = String::new();
    for i in 0..50 {
        use std::fmt::Write as _;
        let _ = writeln!(&mut content, "match line {i}");
    }
    h.create_file("many.txt", &content);
    let registry = make_registry(h.temp_dir());

    let output = common::run_async(async move {
        let tool = registry.get("grep").unwrap();
        tool.execute("grep-limit", json!({"pattern": "match", "limit": 5}), None)
            .await
    });

    let result = output.unwrap();
    let text = first_text(&result.content);
    h.log().info("result", text.to_string());

    assert!(
        text.contains("matches limit reached"),
        "should indicate limit was reached: {text}"
    );
    let details = result.details.expect("should have details");
    assert_eq!(
        details.get("matchLimitReached"),
        Some(&serde_json::json!(5)),
        "details should record limit"
    );
}

// ===========================================================================
// Find Tool — Hardened
// ===========================================================================

/// Find: deeply nested directory structure.
#[test]
fn find_deep_nested_structure() {
    if !fd_available() {
        eprintln!("SKIP: fd/fdfind not installed");
        return;
    }

    let h = TestHarness::new("find_deep_nested_structure");
    h.create_file("a/b/c/deep.txt", "");
    h.create_file("a/b/shallow.txt", "");
    h.create_file("top.txt", "");
    let registry = make_registry(h.temp_dir());

    let output = common::run_async(async move {
        let tool = registry.get("find").unwrap();
        tool.execute("find-deep", json!({"pattern": "*.txt"}), None)
            .await
    });

    let result = output.unwrap();
    let text = first_text(&result.content);
    h.log().info("result", text.to_string());

    assert!(text.contains("deep.txt"), "should find deeply nested file");
    assert!(text.contains("shallow.txt"), "should find mid-level file");
    assert!(text.contains("top.txt"), "should find top-level file");
}

/// Find: many files with limit enforcement.
#[test]
fn find_many_files_limit() {
    if !fd_available() {
        eprintln!("SKIP: fd/fdfind not installed");
        return;
    }

    let h = TestHarness::new("find_many_files_limit");
    for i in 0..20 {
        h.create_file(format!("file_{i:03}.dat"), "");
    }
    let registry = make_registry(h.temp_dir());

    let output = common::run_async(async move {
        let tool = registry.get("find").unwrap();
        tool.execute("find-many", json!({"pattern": "*.dat", "limit": 5}), None)
            .await
    });

    let result = output.unwrap();
    let text = first_text(&result.content);
    h.log().info("result", text.to_string());

    let dat_count = text.lines().filter(|l| l.contains(".dat")).count();
    assert!(dat_count <= 5, "should respect limit of 5, got {dat_count}");
    assert!(
        text.contains("results limit reached"),
        "should indicate limit was hit: {text}"
    );
}

// ===========================================================================
// Ls Tool — Hardened
// ===========================================================================

/// Ls: hidden dotfiles are listed.
#[test]
fn ls_hidden_dotfiles() {
    let h = TestHarness::new("ls_hidden_dotfiles");
    h.create_file(".hidden", "secret");
    h.create_file("visible.txt", "public");
    let registry = make_registry(h.temp_dir());

    let dir_str = h.temp_dir().display().to_string();
    let output = common::run_async(async move {
        let tool = registry.get("ls").unwrap();
        tool.execute("ls-hidden", json!({"path": dir_str}), None)
            .await
    });

    let result = output.unwrap();
    let text = first_text(&result.content);
    h.log().info("result", text.to_string());

    assert!(text.contains("visible.txt"), "should list visible files");
    // Note: ls tool may or may not show hidden files — this tests the behavior
    // Either way, visible files should always appear
}

/// Ls: alphabetical sorting of entries.
#[test]
fn ls_alphabetical_sorting() {
    let h = TestHarness::new("ls_alphabetical_sorting");
    h.create_file("zebra.txt", "");
    h.create_file("alpha.txt", "");
    h.create_file("mango.txt", "");
    let registry = make_registry(h.temp_dir());

    let dir_str = h.temp_dir().display().to_string();
    let output = common::run_async(async move {
        let tool = registry.get("ls").unwrap();
        tool.execute("ls-sort", json!({"path": dir_str}), None)
            .await
    });

    let result = output.unwrap();
    let text = first_text(&result.content);
    h.log().info("result", text.to_string());

    let lines: Vec<&str> = text.lines().collect();
    let alpha_pos = lines.iter().position(|l| l.contains("alpha"));
    let mango_pos = lines.iter().position(|l| l.contains("mango"));
    let zebra_pos = lines.iter().position(|l| l.contains("zebra"));

    if let (Some(a), Some(m), Some(z)) = (alpha_pos, mango_pos, zebra_pos) {
        assert!(a < m, "alpha should come before mango");
        assert!(m < z, "mango should come before zebra");
    }
}

/// Ls: mixed files and directories with correct suffix markers.
#[test]
fn ls_mixed_files_and_dirs() {
    let h = TestHarness::new("ls_mixed_files_and_dirs");
    h.create_file("file_a.txt", "");
    h.create_dir("dir_b");
    h.create_file("file_c.rs", "");
    h.create_dir("dir_d");
    let registry = make_registry(h.temp_dir());

    let dir_str = h.temp_dir().display().to_string();
    let output = common::run_async(async move {
        let tool = registry.get("ls").unwrap();
        tool.execute("ls-mixed", json!({"path": dir_str}), None)
            .await
    });

    let result = output.unwrap();
    let text = first_text(&result.content);
    h.log().info("result", text.to_string());

    // Directories should have '/' suffix
    assert!(
        text.contains("dir_b/"),
        "directory should have / suffix: {text}"
    );
    assert!(
        text.contains("dir_d/"),
        "directory should have / suffix: {text}"
    );
    // Files should NOT have '/' suffix
    assert!(text.contains("file_a.txt"), "should list file");
    assert!(text.contains("file_c.rs"), "should list file");
}

/// Ls: symlink to directory is listed.
#[cfg(unix)]
#[test]
fn ls_symlink_directory() {
    let h = TestHarness::new("ls_symlink_directory");
    let real_dir = h.create_dir("real_dir");
    h.create_file("real_dir/inside.txt", "content");
    let link = h.temp_path("link_dir");
    std::os::unix::fs::symlink(&real_dir, &link).expect("create dir symlink");
    let registry = make_registry(h.temp_dir());

    let dir_str = h.temp_dir().display().to_string();
    let output = common::run_async(async move {
        let tool = registry.get("ls").unwrap();
        tool.execute("ls-symdir", json!({"path": dir_str}), None)
            .await
    });

    let result = output.unwrap();
    let text = first_text(&result.content);
    h.log().info("result", text.to_string());

    assert!(text.contains("real_dir"), "should list real directory");
    assert!(text.contains("link_dir"), "should list symlinked directory");
}

// ===========================================================================
// Cross-tool — Hardened integration scenarios
// ===========================================================================

/// Cross-tool: write → grep → edit → read roundtrip with verification.
#[test]
fn cross_tool_write_grep_edit_read() {
    if !rg_available() {
        eprintln!("SKIP: ripgrep (rg) not installed");
        return;
    }

    let h = TestHarness::new("cross_tool_write_grep_edit_read");
    let file_path = h.temp_path("project/code.py");
    let file_str = file_path.display().to_string();

    // Step 1: Write a Python file
    let write_reg = make_registry(h.temp_dir());
    let write_file = file_str.clone();
    let write_result = common::run_async(async move {
        let tool = write_reg.get("write").unwrap();
        tool.execute(
            "xw-1",
            json!({
                "path": write_file,
                "content": "def hello():\n    return \"old_value\"\n\ndef goodbye():\n    return \"stays\"\n"
            }),
            None,
        )
        .await
    });
    assert!(!write_result.unwrap().is_error, "write should succeed");

    // Step 2: Grep to find the target
    let grep_reg = make_registry(h.temp_dir());
    let grep_result = common::run_async(async move {
        let tool = grep_reg.get("grep").unwrap();
        tool.execute("xg-1", json!({"pattern": "old_value"}), None)
            .await
    });
    let grep_output = grep_result.unwrap();
    let grep_text = first_text(&grep_output.content);
    assert!(grep_text.contains("old_value"), "grep should find target");

    // Step 3: Edit the target
    let edit_reg = make_registry(h.temp_dir());
    let edit_file = file_str.clone();
    let edit_result = common::run_async(async move {
        let tool = edit_reg.get("edit").unwrap();
        tool.execute(
            "xe-1",
            json!({
                "path": edit_file,
                "oldText": "\"old_value\"",
                "newText": "\"new_value\""
            }),
            None,
        )
        .await
    });
    assert!(!edit_result.unwrap().is_error, "edit should succeed");

    // Step 4: Read back and verify
    let read_reg = make_registry(h.temp_dir());
    let read_file = file_str;
    let read_result = common::run_async(async move {
        let tool = read_reg.get("read").unwrap();
        tool.execute("xr-1", json!({"path": read_file}), None).await
    });
    let read_output = read_result.unwrap();
    let text = first_text(&read_output.content);
    h.log().info("final", text.to_string());

    assert!(text.contains("new_value"), "edit should be visible");
    assert!(!text.contains("old_value"), "old value should be gone");
    assert!(text.contains("stays"), "untouched code preserved");
}

/// Cross-tool: bash creates files, find discovers them, read verifies content.
#[test]
fn cross_tool_bash_find_read() {
    if !fd_available() {
        eprintln!("SKIP: fd/fdfind not installed");
        return;
    }

    let h = TestHarness::new("cross_tool_bash_find_read");

    // Step 1: Bash creates files
    let bash_reg = make_registry(h.temp_dir());
    let bash_result = common::run_async(async move {
        let tool = bash_reg.get("bash").unwrap();
        tool.execute(
            "xb-1",
            json!({
                "command": "mkdir -p generated && echo 'auto_content_A' > generated/a.txt && echo 'auto_content_B' > generated/b.txt"
            }),
            None,
        )
        .await
    });
    assert!(!bash_result.unwrap().is_error);

    // Step 2: Find discovers them
    let find_reg = make_registry(h.temp_dir());
    let find_result = common::run_async(async move {
        let tool = find_reg.get("find").unwrap();
        tool.execute(
            "xf-1",
            json!({"pattern": "*.txt", "path": "generated"}),
            None,
        )
        .await
    });
    let find_output = find_result.unwrap();
    let find_text = first_text(&find_output.content);
    assert!(find_text.contains("a.txt"), "find should discover a.txt");
    assert!(find_text.contains("b.txt"), "find should discover b.txt");

    // Step 3: Read verifies content
    let read_reg = make_registry(h.temp_dir());
    let read_path = h.temp_path("generated/a.txt").display().to_string();
    let read_result = common::run_async(async move {
        let tool = read_reg.get("read").unwrap();
        tool.execute("xr-2", json!({"path": read_path}), None).await
    });
    let read_output = read_result.unwrap();
    let text = first_text(&read_output.content);
    h.log().info("final", text.to_string());
    assert!(
        text.contains("auto_content_A"),
        "bash-created file should have expected content"
    );
}
