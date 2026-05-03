//! Built-in tool implementations.
//!
//! Pi provides 8 built-in tools: read, bash, edit, write, grep, find, ls, hashline_edit.
//!
//! Tools are exposed to the model via JSON Schema (see [`crate::provider::ToolDef`]) and executed
//! locally by the agent loop. Each tool returns structured [`ContentBlock`] output suitable for
//! rendering in the TUI and for inclusion in provider messages as tool results.

use crate::agent_cx::AgentCx;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::extensions::{safe_canonicalize, strip_unc_prefix};
use crate::model::{ContentBlock, ImageContent, TextContent};
use asupersync::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, ReadBuf, SeekFrom};
use asupersync::time::{sleep, wall_now};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::{HashMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{OnceLock, mpsc};
use std::thread;
use std::time::{Duration, SystemTime};
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

// ============================================================================
// Tool Trait
// ============================================================================

/// A tool that can be executed by the agent.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Get the tool name.
    fn name(&self) -> &str;

    /// Get the tool label (display name).
    fn label(&self) -> &str;

    /// Get the tool description.
    fn description(&self) -> &str;

    /// Get the tool parameters as JSON Schema.
    fn parameters(&self) -> serde_json::Value;

    /// Execute the tool.
    ///
    /// Tools may call `on_update` to stream incremental results (e.g. while a long-running `bash`
    /// command is still producing output). The final return value is a [`ToolOutput`] which is
    /// persisted into the session as a tool result message.
    async fn execute(
        &self,
        tool_call_id: &str,
        input: serde_json::Value,
        on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput>;

    /// Whether the tool is read-only and safe to execute in parallel with other read-only tools.
    ///
    /// Defaults to `false` (safe/sequential).
    fn is_read_only(&self) -> bool {
        false
    }
}

/// Tool execution output.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolOutput {
    pub content: Vec<ContentBlock>,
    pub details: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_error: bool,
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde requires `fn(&bool) -> bool` for `skip_serializing_if`
const fn is_false(value: &bool) -> bool {
    !*value
}

/// Incremental update during tool execution.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolUpdate {
    pub content: Vec<ContentBlock>,
    pub details: Option<serde_json::Value>,
}

// ============================================================================
// Truncation
// ============================================================================

/// Default maximum lines for truncation.
pub const DEFAULT_MAX_LINES: usize = 2000;

/// Default maximum bytes for truncation.
pub const DEFAULT_MAX_BYTES: usize = 1_000_000; // 1MB

/// Maximum line length for grep results.
pub const GREP_MAX_LINE_LENGTH: usize = 500;

/// Default grep result limit.
pub const DEFAULT_GREP_LIMIT: usize = 100;

/// Default find result limit.
pub const DEFAULT_FIND_LIMIT: usize = 1000;

/// Default ls result limit.
pub const DEFAULT_LS_LIMIT: usize = 500;

/// Hard limit for directory scanning in ls tool to prevent OOM/hangs.
pub const LS_SCAN_HARD_LIMIT: usize = 20_000;

/// Hard limit for read tool file size (100MB) to prevent OOM.
pub const READ_TOOL_MAX_BYTES: u64 = 100 * 1024 * 1024;

/// Hard limit for write/edit tool file size (100MB) to prevent OOM.
pub const WRITE_TOOL_MAX_BYTES: usize = 100 * 1024 * 1024;

/// Maximum size for an image to be sent to the API (4.5MB).
pub const IMAGE_MAX_BYTES: usize = 4_718_592;

/// Default timeout (in seconds) for bash tool execution.
pub const DEFAULT_BASH_TIMEOUT_SECS: u64 = 120;

const BASH_TERMINATE_GRACE_SECS: u64 = 5;

/// Hard limit for bash output file size (1GB) to prevent disk exhaustion DoS.
pub(crate) const BASH_FILE_LIMIT_BYTES: usize = 1024 * 1024 * 1024; // 1 GiB

/// Result of truncation operation.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TruncationResult {
    pub content: String,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated_by: Option<TruncatedBy>,
    pub total_lines: usize,
    pub total_bytes: usize,
    pub output_lines: usize,
    pub output_bytes: usize,
    pub last_line_partial: bool,
    pub first_line_exceeds_limit: bool,
    pub max_lines: usize,
    pub max_bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum TruncatedBy {
    Lines,
    Bytes,
}

/// Truncate from the beginning (keep first N lines).
///
/// Takes ownership of the input `String` to avoid allocation in the common
/// no-truncation case (content moved, zero-copy) and to enable in-place
/// truncation when the content exceeds limits (`String::truncate`, no new
/// allocation).
#[allow(clippy::too_many_lines)]
pub fn truncate_head(
    content: impl Into<String>,
    max_lines: usize,
    max_bytes: usize,
) -> TruncationResult {
    let mut content = content.into();
    let total_bytes = content.len();

    let total_lines = {
        let nl = memchr::memchr_iter(b'\n', content.as_bytes()).count();
        if content.is_empty() {
            0
        } else if content.ends_with('\n') {
            nl
        } else {
            nl + 1
        }
    };

    if max_lines == 0 {
        let truncated = !content.is_empty();
        content.truncate(0);
        return TruncationResult {
            content,
            truncated,
            truncated_by: if truncated {
                Some(TruncatedBy::Lines)
            } else {
                None
            },
            total_lines,
            total_bytes,
            output_lines: 0,
            output_bytes: 0,
            last_line_partial: false,
            first_line_exceeds_limit: false,
            max_lines,
            max_bytes,
        };
    }

    if max_bytes == 0 {
        let truncated = !content.is_empty();
        let first_line_exceeds_limit = !content.is_empty();
        content.truncate(0);
        return TruncationResult {
            content,
            truncated,
            truncated_by: if truncated {
                Some(TruncatedBy::Bytes)
            } else {
                None
            },
            total_lines,
            total_bytes,
            output_lines: 0,
            output_bytes: 0,
            last_line_partial: false,
            first_line_exceeds_limit,
            max_lines,
            max_bytes,
        };
    }

    if total_lines <= max_lines && total_bytes <= max_bytes {
        return TruncationResult {
            content,
            truncated: false,
            truncated_by: None,
            total_lines,
            total_bytes,
            output_lines: total_lines,
            output_bytes: total_bytes,
            last_line_partial: false,
            first_line_exceeds_limit: false,
            max_lines,
            max_bytes,
        };
    }

    let first_newline = memchr::memchr(b'\n', content.as_bytes());
    let first_line_bytes = first_newline.unwrap_or(content.len());

    if first_line_bytes > max_bytes {
        let mut valid_bytes = max_bytes;
        while valid_bytes > 0 && !content.is_char_boundary(valid_bytes) {
            valid_bytes -= 1;
        }
        content.truncate(valid_bytes);
        return TruncationResult {
            content,
            truncated: true,
            truncated_by: Some(TruncatedBy::Bytes),
            total_lines,
            total_bytes,
            output_lines: usize::from(valid_bytes > 0),
            output_bytes: valid_bytes,
            last_line_partial: true,
            first_line_exceeds_limit: true,
            max_lines,
            max_bytes,
        };
    }

    let mut line_count = 0;
    let mut byte_count = 0;
    let mut truncated_by = None;
    let mut current_offset = 0;
    let mut last_line_partial = false;

    while current_offset < content.len() {
        if line_count >= max_lines {
            truncated_by = Some(TruncatedBy::Lines);
            break;
        }

        let next_newline = memchr::memchr(b'\n', &content.as_bytes()[current_offset..]);
        let line_end_without_nl = next_newline.map_or(content.len(), |idx| current_offset + idx);
        let line_end_with_nl = next_newline.map_or(content.len(), |idx| current_offset + idx + 1);

        if line_end_without_nl > max_bytes {
            let mut byte_limit = max_bytes.min(content.len());
            if byte_limit < current_offset {
                truncated_by = Some(TruncatedBy::Bytes);
                break;
            }
            while byte_limit > current_offset && !content.is_char_boundary(byte_limit) {
                byte_limit -= 1;
            }
            if byte_limit > current_offset {
                byte_count = byte_limit;
                line_count += 1;
                last_line_partial = true;
            }
            truncated_by = Some(TruncatedBy::Bytes);
            break;
        }

        if line_end_with_nl > max_bytes {
            if line_end_without_nl > current_offset {
                byte_count = line_end_without_nl;
                line_count += 1;
            }
            truncated_by = Some(TruncatedBy::Bytes);
            break;
        }

        byte_count = line_end_with_nl;
        line_count += 1;
        current_offset = line_end_with_nl;
    }

    content.truncate(byte_count);

    TruncationResult {
        truncated: truncated_by.is_some(),
        truncated_by,
        total_lines,
        total_bytes,
        output_lines: line_count,
        output_bytes: byte_count,
        last_line_partial,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
        content,
    }
}

/// Truncate from the end (keep last N lines).
///
/// Takes ownership of the input `String` to avoid allocation in the common
/// no-truncation case (content moved, zero-copy). When truncation is needed,
/// the prefix is drained in-place, reusing the original buffer.
#[allow(clippy::too_many_lines)]
pub fn truncate_tail(
    content: impl Into<String>,
    max_lines: usize,
    max_bytes: usize,
) -> TruncationResult {
    let mut content = content.into();
    let total_bytes = content.len();

    // Count lines correctly: trailing newline terminates the last line, it doesn't start a new one.
    // "a\n" -> 1 line. "a\nb" -> 2 lines. "a" -> 1 line. "" -> 0 lines (handled below).
    let mut total_lines = memchr::memchr_iter(b'\n', content.as_bytes()).count();
    if !content.ends_with('\n') && !content.is_empty() {
        total_lines += 1;
    }
    if content.is_empty() {
        total_lines = 0;
    }

    // Explicitly handle zero-line budgets. Keeping any line would violate the
    // contract (`output_lines <= max_lines`) and proptest invariants.
    if max_lines == 0 {
        let truncated = !content.is_empty();
        return TruncationResult {
            content: String::new(),
            truncated,
            truncated_by: if truncated {
                Some(TruncatedBy::Lines)
            } else {
                None
            },
            total_lines,
            total_bytes,
            output_lines: 0,
            output_bytes: 0,
            last_line_partial: false,
            first_line_exceeds_limit: false,
            max_lines,
            max_bytes,
        };
    }

    // No truncation needed — reuse the owned String (zero-copy move).
    if total_lines <= max_lines && total_bytes <= max_bytes {
        return TruncationResult {
            content,
            truncated: false,
            truncated_by: None,
            total_lines,
            total_bytes,
            output_lines: total_lines,
            output_bytes: total_bytes,
            last_line_partial: false,
            first_line_exceeds_limit: false,
            max_lines,
            max_bytes,
        };
    }

    let mut line_count = 0usize;
    let mut byte_count = 0usize;
    let mut start_idx = content.len();
    let mut partial_output: Option<String> = None;
    let mut partial_line_truncated = false;
    let mut truncated_by = None;
    let mut last_line_partial = false;

    // Scope the immutable borrow so we can mutate `content` afterwards.
    {
        let bytes = content.as_bytes();
        // Initialize search_limit outside the loop to track progress backwards.
        // If the file ends with a newline, we skip it for the purpose of finding
        // the *start* of the last line, but start_idx (at len) includes it.
        let mut search_limit = bytes.len();
        if search_limit > 0 && bytes[search_limit - 1] == b'\n' {
            search_limit -= 1;
        }

        loop {
            // Find the *previous* newline.
            let prev_newline = memchr::memrchr(b'\n', &bytes[..search_limit]);
            let line_start = prev_newline.map_or(0, |idx| idx + 1);

            // Bytes for this line (including its newline if it's not the last one,
            // or if the file ends with newline). start_idx is the end of the
            // segment we are accumulating.
            let added_bytes = start_idx - line_start;

            if byte_count + added_bytes > max_bytes {
                // Try to take a partial line if byte budget remains. This
                // preserves suffix stability under prepends while staying on a
                // valid UTF-8 boundary.
                let remaining = max_bytes.saturating_sub(byte_count);
                if remaining > 0 {
                    let chunk = &content[line_start..start_idx];
                    let truncated_chunk = truncate_string_to_bytes_from_end(chunk, remaining);
                    if !truncated_chunk.is_empty() {
                        partial_output = Some(truncated_chunk);
                        partial_line_truncated = true;
                        if line_count == 0 {
                            last_line_partial = true;
                        }
                    }
                }
                truncated_by = Some(TruncatedBy::Bytes);
                break;
            }

            line_count += 1;
            byte_count += added_bytes;
            start_idx = line_start;

            if line_count >= max_lines {
                truncated_by = Some(TruncatedBy::Lines);
                break;
            }

            if line_start == 0 {
                break;
            }

            // Prepare for next iter.
            // We just consumed line starting at `line_start`.
            // The separator before it is at `line_start - 1`.
            // That separator is the `\n` of the *previous* line.
            // We want to search *before* it.
            search_limit = line_start - 1;
        }
    } // immutable borrow of `content` released

    // Extract the suffix: drain the prefix in-place (reuses the buffer),
    // or use the partial output from the byte-truncation path.
    let partial_suffix = if partial_line_truncated {
        Some(content[start_idx..].to_string())
    } else {
        None
    };

    let mut output = partial_output.unwrap_or_else(|| {
        drop(content.drain(..start_idx));
        content
    });

    // If we have a partial last line, we need to append the *rest* of the content
    // that we successfully kept (the `byte_count` lines).
    // Wait, `partial_output` replaces the *current line*.
    // The previous successful lines are in `content[old_start_idx..]`.
    // My logic above for partial output:
    // `truncated_chunk` is the partial tail of the *current line*.
    // We need to prepend it to the lines we already collected?
    // Actually, `content` is the full string.
    // We are scanning backwards.
    // `start_idx` tracks the start of the valid suffix so far.
    // When we hit the byte limit, we are at `line_start..start_idx`.
    // `truncated_chunk` is the tail of *that* segment.
    // So final output = `truncated_chunk` + `content[start_idx..]`.

    if let Some(suffix) = partial_suffix {
        // Need to reconstruct.
        // `output` is currently just the truncated chunk.
        // We need to append the previously accumulated suffix.
        // `content` still holds everything.
        // `start_idx` points to the start of the *valid* suffix from previous iters.
        output.push_str(&suffix);
        // Recalculate line count from the final output.
        // Since truncated output is bounded (<= max_bytes), this scan is cheap.
        let mut count = memchr::memchr_iter(b'\n', output.as_bytes()).count();
        if !output.ends_with('\n') && !output.is_empty() {
            count += 1;
        }
        if output.is_empty() {
            count = 0;
        }
        line_count = count;
    }

    let output_bytes = output.len();

    TruncationResult {
        content: output,
        truncated: truncated_by.is_some(),
        truncated_by,
        total_lines,
        total_bytes,
        output_lines: line_count,
        output_bytes,
        last_line_partial,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    }
}

/// Truncate a string to fit within a byte limit (from the end), preserving UTF-8 boundaries.
fn truncate_string_to_bytes_from_end(s: &str, max_bytes: usize) -> String {
    let bytes = s.as_bytes();
    if bytes.len() <= max_bytes {
        return s.to_string();
    }

    let mut start = bytes.len().saturating_sub(max_bytes);
    while start < bytes.len() && (bytes[start] & 0b1100_0000) == 0b1000_0000 {
        start += 1;
    }

    std::str::from_utf8(&bytes[start..])
        .map(str::to_string)
        .unwrap_or_default()
}

/// Format a byte count into a human-readable string with appropriate unit suffix.
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

fn js_string_length(s: &str) -> usize {
    // Match JavaScript's String.length (UTF-16 code units), not UTF-8 bytes.
    s.encode_utf16().count()
}

// ============================================================================
// Path Utilities (port of pi-mono path-utils.ts)
// ============================================================================

fn is_special_unicode_space(c: char) -> bool {
    matches!(c, '\u{00A0}' | '\u{202F}' | '\u{205F}' | '\u{3000}')
        || ('\u{2000}'..='\u{200A}').contains(&c)
}

fn normalize_unicode_spaces(s: &str) -> String {
    s.chars()
        .map(|c| if is_special_unicode_space(c) { ' ' } else { c })
        .collect()
}

fn normalize_quotes(s: &str) -> String {
    s.replace(['\u{2018}', '\u{2019}'], "'")
        .replace(['\u{201C}', '\u{201D}', '\u{201E}', '\u{201F}'], "\"")
}

fn normalize_dashes(s: &str) -> String {
    s.replace(
        [
            '\u{2010}', '\u{2011}', '\u{2012}', '\u{2013}', '\u{2014}', '\u{2015}', '\u{2212}',
        ],
        "-",
    )
}

fn normalize_for_match(s: &str) -> String {
    // Single-pass normalization: spaces, quotes, and dashes in one allocation.
    // Avoids 3 intermediate String allocations from chained replace calls.
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            // Unicode spaces → ASCII space
            c if is_special_unicode_space(c) => out.push(' '),
            // Curly single quotes → straight apostrophe
            '\u{2018}' | '\u{2019}' => out.push('\''),
            // Curly double quotes → straight double quote
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => out.push('"'),
            // Various dashes → ASCII hyphen
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
            | '\u{2212}' => out.push('-'),
            // Everything else passes through
            c => out.push(c),
        }
    }
    out
}

fn normalize_line_for_match(line: &str) -> String {
    normalize_for_match(line.trim_end())
}

fn expand_path(file_path: &str) -> String {
    let normalized = normalize_unicode_spaces(file_path);
    if normalized == "~" {
        return dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("~"))
            .to_string_lossy()
            .to_string();
    }
    if let Some(rest) = normalized.strip_prefix("~/") {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("~"));
        return home.join(rest).to_string_lossy().to_string();
    }
    normalized
}

/// Resolve a path relative to `cwd`. Handles `~` expansion and absolute paths.
fn resolve_to_cwd(file_path: &str, cwd: &Path) -> PathBuf {
    let expanded = expand_path(file_path);
    let expanded_path = PathBuf::from(expanded);
    if expanded_path.is_absolute() {
        expanded_path
    } else {
        cwd.join(expanded_path)
    }
}

fn try_mac_os_screenshot_path(file_path: &str) -> String {
    // Replace " AM." / " PM." with a narrow no-break space variant used by macOS screenshots.
    file_path
        .replace(" AM.", "\u{202F}AM.")
        .replace(" PM.", "\u{202F}PM.")
}

fn try_curly_quote_variant(file_path: &str) -> String {
    // Replace straight apostrophe with macOS screenshot curly apostrophe.
    file_path.replace('\'', "\u{2019}")
}

fn try_nfd_variant(file_path: &str) -> String {
    // NFD normalization - decompose characters into base + combining marks
    // This handles macOS HFS+ filesystem normalization differences
    use unicode_normalization::UnicodeNormalization;
    file_path.nfd().collect::<String>()
}

fn file_exists(path: &Path) -> bool {
    std::fs::metadata(path).is_ok()
}

/// Resolve a file path for reading, including macOS screenshot name variants.
pub(crate) fn resolve_read_path(file_path: &str, cwd: &Path) -> PathBuf {
    let resolved = normalize_dot_segments(&resolve_to_cwd(file_path, cwd));
    let normalized_cwd = normalize_dot_segments(cwd);
    let within_cwd = resolved.starts_with(&normalized_cwd);
    if within_cwd && file_exists(&resolved) {
        return resolved;
    }
    if !within_cwd {
        // Avoid probing the filesystem outside the working directory.
        return resolved;
    }

    let Some(resolved_str) = resolved.to_str() else {
        return resolved;
    };

    let am_pm_variant = try_mac_os_screenshot_path(resolved_str);
    if am_pm_variant != resolved_str {
        let candidate = PathBuf::from(&am_pm_variant);
        if candidate.starts_with(&normalized_cwd) && file_exists(&candidate) {
            return candidate;
        }
    }

    let nfd_variant = try_nfd_variant(resolved_str);
    if nfd_variant != resolved_str {
        let candidate = PathBuf::from(&nfd_variant);
        if candidate.starts_with(&normalized_cwd) && file_exists(&candidate) {
            return candidate;
        }
    }

    let curly_variant = try_curly_quote_variant(resolved_str);
    if curly_variant != resolved_str {
        let candidate = PathBuf::from(&curly_variant);
        if candidate.starts_with(&normalized_cwd) && file_exists(&candidate) {
            return candidate;
        }
    }

    let nfd_curly_variant = try_curly_quote_variant(&nfd_variant);
    if nfd_curly_variant != resolved_str {
        let candidate = PathBuf::from(&nfd_curly_variant);
        if candidate.starts_with(&normalized_cwd) && file_exists(&candidate) {
            return candidate;
        }
    }

    resolved
}

fn enforce_cwd_scope(path: &Path, cwd: &Path, action: &str) -> Result<PathBuf> {
    let canonical_path = crate::extensions::safe_canonicalize(path);
    let canonical_cwd = crate::extensions::safe_canonicalize(cwd);
    if !canonical_path.starts_with(&canonical_cwd) {
        return Err(Error::validation(format!(
            "Cannot {action} outside the working directory (resolved: {}, cwd: {})",
            canonical_path.display(),
            canonical_cwd.display()
        )));
    }
    Ok(canonical_path)
}

/// Same as `enforce_cwd_scope` but also permits paths under the supplied
/// agent directory (typically `~/.pi/agent/`, overridable via
/// PI_CODING_AGENT_DIR).
///
/// Used for the `read` tool so the agent can load its own resource files
/// (skills, prompt templates, themes) which live under the agent dir, not
/// under the user's project cwd. Symlink escapes are still blocked because
/// `safe_canonicalize` resolves symlinks before the prefix check.
///
/// Other tools (write, edit, grep, find, ls, hashline_edit) keep using
/// `enforce_cwd_scope` and remain strictly cwd-only — broadening write
/// access would let a misbehaving model persist instructions into the
/// agent dir, which is more risk than the read case warrants.
fn enforce_read_scope_with_agent_dir(
    path: &Path,
    cwd: &Path,
    agent_dir: &Path,
) -> Result<PathBuf> {
    let canonical_path = crate::extensions::safe_canonicalize(path);
    let canonical_cwd = crate::extensions::safe_canonicalize(cwd);
    if canonical_path.starts_with(&canonical_cwd) {
        return Ok(canonical_path);
    }

    let canonical_agent_dir = crate::extensions::safe_canonicalize(agent_dir);
    if canonical_path.starts_with(&canonical_agent_dir) {
        return Ok(canonical_path);
    }

    Err(Error::validation(format!(
        "Cannot read outside the working directory or agent dir \
         (resolved: {}, cwd: {}, agent dir: {})",
        canonical_path.display(),
        canonical_cwd.display(),
        canonical_agent_dir.display()
    )))
}

/// Convenience wrapper that pulls the agent dir from the active config.
fn enforce_read_scope(path: &Path, cwd: &Path) -> Result<PathBuf> {
    let agent_dir = crate::config::Config::global_dir();
    enforce_read_scope_with_agent_dir(path, cwd, &agent_dir)
}

// ============================================================================
// CLI @file Processor (used by src/main.rs)
// ============================================================================

/// Result of processing `@file` CLI arguments.
#[derive(Debug, Clone, Default)]
pub struct ProcessedFiles {
    pub text: String,
    pub images: Vec<ImageContent>,
}

fn normalize_dot_segments(path: &Path) -> PathBuf {
    use std::ffi::{OsStr, OsString};
    use std::path::Component;

    let mut out = PathBuf::new();
    let mut normals: Vec<OsString> = Vec::new();
    let mut has_prefix = false;
    let mut has_root = false;

    for component in path.components() {
        match component {
            Component::Prefix(prefix) => {
                out.push(prefix.as_os_str());
                has_prefix = true;
            }
            Component::RootDir => {
                out.push(component.as_os_str());
                has_root = true;
            }
            Component::CurDir => {}
            Component::ParentDir => match normals.last() {
                Some(last) if last.as_os_str() != OsStr::new("..") => {
                    normals.pop();
                }
                _ => {
                    if !has_root && !has_prefix {
                        normals.push(OsString::from(".."));
                    }
                }
            },
            Component::Normal(part) => normals.push(part.to_os_string()),
        }
    }

    for part in normals {
        out.push(part);
    }

    out
}

#[cfg(feature = "fuzzing")]
pub fn fuzz_normalize_dot_segments(path: &Path) -> PathBuf {
    normalize_dot_segments(path)
}

#[cfg(unix)]
fn sync_parent_dir(path: &Path) -> std::io::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    std::fs::File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_dir(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

fn escape_file_tag_attribute(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '"' => escaped.push_str("&quot;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '\n' => escaped.push_str("&#10;"),
            '\r' => escaped.push_str("&#13;"),
            '\t' => escaped.push_str("&#9;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

fn escaped_file_tag_name(path: &Path) -> String {
    escape_file_tag_attribute(&path.display().to_string())
}

fn append_file_notice_block(out: &mut String, path: &Path, notice: &str) {
    let path_str = escaped_file_tag_name(path);
    let _ = writeln!(out, "<file name=\"{path_str}\">\n{notice}\n</file>");
}

fn append_image_file_ref(out: &mut String, path: &Path, note: Option<&str>) {
    let path_str = escaped_file_tag_name(path);
    match note {
        Some(text) => {
            let _ = writeln!(out, "<file name=\"{path_str}\">{text}</file>");
        }
        None => {
            let _ = writeln!(out, "<file name=\"{path_str}\"></file>");
        }
    }
}

fn append_text_file_block(out: &mut String, path: &Path, bytes: &[u8]) {
    let content = String::from_utf8_lossy(bytes);
    let path_str = escaped_file_tag_name(path);
    let _ = writeln!(out, "<file name=\"{path_str}\">");

    let truncation = truncate_head(content.into_owned(), DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
    let needs_trailing_newline = !truncation.truncated && !truncation.content.ends_with('\n');
    out.push_str(&truncation.content);

    if truncation.truncated {
        let _ = write!(
            out,
            "\n... [Truncated: showing {}/{} lines, {}/{} bytes]",
            truncation.output_lines,
            truncation.total_lines,
            format_size(truncation.output_bytes),
            format_size(truncation.total_bytes)
        );
    } else if needs_trailing_newline {
        out.push('\n');
    }
    let _ = writeln!(out, "</file>");
}

fn maybe_append_image_argument(
    out: &mut ProcessedFiles,
    absolute_path: &Path,
    bytes: &[u8],
    auto_resize_images: bool,
) -> Result<bool> {
    let Some(mime_type) = detect_supported_image_mime_type_from_bytes(bytes) else {
        return Ok(false);
    };

    let resized = if auto_resize_images {
        resize_image_if_needed(bytes, mime_type)?
    } else {
        ResizedImage::original(bytes.to_vec(), mime_type)
    };

    if resized.bytes.len() > IMAGE_MAX_BYTES {
        let msg = if resized.resized {
            format!(
                "[Image is too large ({} bytes) after resizing. Max allowed is {} bytes.]",
                resized.bytes.len(),
                IMAGE_MAX_BYTES
            )
        } else {
            format!(
                "[Image is too large ({} bytes). Max allowed is {} bytes.]",
                resized.bytes.len(),
                IMAGE_MAX_BYTES
            )
        };
        append_file_notice_block(&mut out.text, absolute_path, &msg);
        return Ok(true);
    }

    let base64_data =
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &resized.bytes);
    out.images.push(ImageContent {
        data: base64_data,
        mime_type: resized.mime_type.to_string(),
    });

    let note = if resized.resized {
        if let (Some(ow), Some(oh), Some(w), Some(h)) = (
            resized.original_width,
            resized.original_height,
            resized.width,
            resized.height,
        ) {
            if w > 0 {
                let scale = f64::from(ow) / f64::from(w);
                Some(format!(
                    "[Image: original {ow}x{oh}, displayed at {w}x{h}. Multiply coordinates by {scale:.2} to map to original image.]"
                ))
            } else {
                Some(format!(
                    "[Image: original {ow}x{oh}, displayed at {w}x{h}.]"
                ))
            }
        } else {
            None
        }
    } else {
        None
    };
    append_image_file_ref(&mut out.text, absolute_path, note.as_deref());
    Ok(true)
}

/// Process `@file` arguments into a single text prefix and image attachments.
///
/// Matches the legacy TypeScript behavior:
/// - Resolves paths (including `~` expansion + macOS screenshot variants)
/// - Skips empty files
/// - For images: attaches image blocks and appends `<file name="...">...</file>` references
/// - For text: embeds the file contents inside `<file>` tags
pub fn process_file_arguments(
    file_args: &[String],
    cwd: &Path,
    auto_resize_images: bool,
) -> Result<ProcessedFiles> {
    let mut out = ProcessedFiles::default();

    for file_arg in file_args {
        let resolved = resolve_read_path(file_arg, cwd);
        let absolute_path = normalize_dot_segments(&resolved);
        let absolute_path = enforce_read_scope(&absolute_path, cwd)?;

        let meta = std::fs::metadata(&absolute_path).map_err(|e| {
            Error::tool(
                "read",
                format!("Cannot access file {}: {e}", absolute_path.display()),
            )
        })?;
        if meta.is_dir() {
            append_file_notice_block(
                &mut out.text,
                &absolute_path,
                "[Path is a directory, not a file. Use the list tool to view its contents.]",
            );
            continue;
        }

        if meta.len() == 0 {
            continue;
        }

        if meta.len() > READ_TOOL_MAX_BYTES {
            append_file_notice_block(
                &mut out.text,
                &absolute_path,
                &format!(
                    "[File is too large ({} bytes). Max allowed is {} bytes.]",
                    meta.len(),
                    READ_TOOL_MAX_BYTES
                ),
            );
            continue;
        }

        let bytes = std::fs::read(&absolute_path).map_err(|e| {
            Error::tool(
                "read",
                format!("Could not read file {}: {e}", absolute_path.display()),
            )
        })?;

        if maybe_append_image_argument(&mut out, &absolute_path, &bytes, auto_resize_images)? {
            continue;
        }

        append_text_file_block(&mut out.text, &absolute_path, &bytes);
    }

    Ok(out)
}

/// Resolve a file path relative to the current working directory.
/// Public alias for `resolve_to_cwd` used by tools.
fn resolve_path(file_path: &str, cwd: &Path) -> PathBuf {
    normalize_dot_segments(&resolve_to_cwd(file_path, cwd))
}

#[cfg(feature = "fuzzing")]
pub fn fuzz_resolve_path(file_path: &str, cwd: &Path) -> PathBuf {
    resolve_path(file_path, cwd)
}

pub(crate) fn detect_supported_image_mime_type_from_bytes(bytes: &[u8]) -> Option<&'static str> {
    // Supported image types match the legacy tool: jpeg/png/gif/webp only.
    if bytes.len() >= 8 && bytes.starts_with(b"\x89PNG\r\n\x1A\n") {
        return Some("image/png");
    }
    if bytes.len() >= 3 && bytes[0] == 0xFF && bytes[1] == 0xD8 && bytes[2] == 0xFF {
        return Some("image/jpeg");
    }
    if bytes.len() >= 6 && (bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")) {
        return Some("image/gif");
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

#[derive(Debug, Clone)]
pub(crate) struct ResizedImage {
    pub(crate) bytes: Vec<u8>,
    pub(crate) mime_type: &'static str,
    pub(crate) resized: bool,
    pub(crate) width: Option<u32>,
    pub(crate) height: Option<u32>,
    pub(crate) original_width: Option<u32>,
    pub(crate) original_height: Option<u32>,
}

impl ResizedImage {
    pub(crate) const fn original(bytes: Vec<u8>, mime_type: &'static str) -> Self {
        Self {
            bytes,
            mime_type,
            resized: false,
            width: None,
            height: None,
            original_width: None,
            original_height: None,
        }
    }
}

#[cfg(feature = "image-resize")]
#[allow(clippy::too_many_lines)]
pub(crate) fn resize_image_if_needed(
    bytes: &[u8],
    mime_type: &'static str,
) -> Result<ResizedImage> {
    // Match legacy behavior from pi-mono `utils/image-resize.ts`.
    //
    // Strategy:
    // 1) If image already fits within max dims AND max bytes: return original
    // 2) Otherwise resize to maxWidth/maxHeight (2000x2000)
    // 3) Encode as PNG and JPEG, pick smaller
    // 4) If still too large, try JPEG with different quality steps
    // 5) If still too large, progressively scale down dimensions
    //
    // Note: even if dimensions don't change, an oversized image may be re-encoded to fit max bytes.
    use image::codecs::jpeg::JpegEncoder;
    use image::codecs::png::PngEncoder;
    use image::imageops::FilterType;
    use image::{GenericImageView, ImageEncoder, ImageReader, Limits};
    use std::io::Cursor;

    const MAX_WIDTH: u32 = 2000;
    const MAX_HEIGHT: u32 = 2000;
    const DEFAULT_JPEG_QUALITY: u8 = 80;
    const QUALITY_STEPS: [u8; 4] = [85, 70, 55, 40];
    const SCALE_STEPS: [f64; 5] = [1.0, 0.75, 0.5, 0.35, 0.25];

    fn scale_u32(value: u32, numerator: u32, denominator: u32) -> u32 {
        let den = u64::from(denominator).max(1);
        let num = u64::from(value) * u64::from(numerator);
        let rounded = (num + den / 2) / den;
        u32::try_from(rounded).unwrap_or(u32::MAX)
    }

    fn encode_png(img: &image::DynamicImage) -> Result<Vec<u8>> {
        let rgba = img.to_rgba8();
        let mut out = Vec::new();
        PngEncoder::new(&mut out)
            .write_image(
                rgba.as_raw(),
                rgba.width(),
                rgba.height(),
                image::ExtendedColorType::Rgba8,
            )
            .map_err(|e| Error::tool("read", format!("Failed to encode PNG: {e}")))?;
        Ok(out)
    }

    fn encode_jpeg(img: &image::DynamicImage, quality: u8) -> Result<Vec<u8>> {
        let rgb = img.to_rgb8();
        let mut out = Vec::new();
        JpegEncoder::new_with_quality(&mut out, quality)
            .write_image(
                rgb.as_raw(),
                rgb.width(),
                rgb.height(),
                image::ExtendedColorType::Rgb8,
            )
            .map_err(|e| Error::tool("read", format!("Failed to encode JPEG: {e}")))?;
        Ok(out)
    }

    fn try_both_formats(
        img: &image::DynamicImage,
        width: u32,
        height: u32,
        jpeg_quality: u8,
    ) -> Result<(Vec<u8>, &'static str)> {
        let resized = img.resize_exact(width, height, FilterType::Lanczos3);
        let png = encode_png(&resized)?;
        let jpeg = encode_jpeg(&resized, jpeg_quality)?;
        if png.len() <= jpeg.len() {
            Ok((png, "image/png"))
        } else {
            Ok((jpeg, "image/jpeg"))
        }
    }

    // Use ImageReader with explicit limits to prevent decompression bomb attacks.
    // 128MB allocation limit allows reasonable images but stops massive expansions.
    let mut limits = Limits::default();
    limits.max_alloc = Some(128 * 1024 * 1024);

    let reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| Error::tool("read", format!("Failed to detect image format: {e}")))?;

    let mut reader = reader;
    reader.limits(limits);

    let Ok(img) = reader.decode() else {
        return Ok(ResizedImage::original(bytes.to_vec(), mime_type));
    };

    let (original_width, original_height) = img.dimensions();
    let original_size = bytes.len();

    if original_width <= MAX_WIDTH
        && original_height <= MAX_HEIGHT
        && original_size <= IMAGE_MAX_BYTES
    {
        return Ok(ResizedImage {
            bytes: bytes.to_vec(),
            mime_type,
            resized: false,
            width: Some(original_width),
            height: Some(original_height),
            original_width: Some(original_width),
            original_height: Some(original_height),
        });
    }

    let mut target_width = original_width;
    let mut target_height = original_height;

    if target_width > MAX_WIDTH {
        target_height = scale_u32(target_height, MAX_WIDTH, target_width);
        target_width = MAX_WIDTH;
    }
    if target_height > MAX_HEIGHT {
        target_width = scale_u32(target_width, MAX_HEIGHT, target_height);
        target_height = MAX_HEIGHT;
    }

    let mut best = try_both_formats(&img, target_width, target_height, DEFAULT_JPEG_QUALITY)?;
    let mut final_width = target_width;
    let mut final_height = target_height;

    if best.0.len() <= IMAGE_MAX_BYTES {
        return Ok(ResizedImage {
            bytes: best.0,
            mime_type: best.1,
            resized: true,
            width: Some(final_width),
            height: Some(final_height),
            original_width: Some(original_width),
            original_height: Some(original_height),
        });
    }

    for quality in QUALITY_STEPS {
        best = try_both_formats(&img, target_width, target_height, quality)?;
        if best.0.len() <= IMAGE_MAX_BYTES {
            return Ok(ResizedImage {
                bytes: best.0,
                mime_type: best.1,
                resized: true,
                width: Some(final_width),
                height: Some(final_height),
                original_width: Some(original_width),
                original_height: Some(original_height),
            });
        }
    }

    for scale in SCALE_STEPS {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        {
            final_width = (f64::from(target_width) * scale).round() as u32;
            final_height = (f64::from(target_height) * scale).round() as u32;
        }

        if final_width < 100 || final_height < 100 {
            break;
        }

        for quality in QUALITY_STEPS {
            best = try_both_formats(&img, final_width, final_height, quality)?;
            if best.0.len() <= IMAGE_MAX_BYTES {
                return Ok(ResizedImage {
                    bytes: best.0,
                    mime_type: best.1,
                    resized: true,
                    width: Some(final_width),
                    height: Some(final_height),
                    original_width: Some(original_width),
                    original_height: Some(original_height),
                });
            }
        }
    }

    Ok(ResizedImage {
        bytes: best.0,
        mime_type: best.1,
        resized: true,
        width: Some(final_width),
        height: Some(final_height),
        original_width: Some(original_width),
        original_height: Some(original_height),
    })
}

#[cfg(not(feature = "image-resize"))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "The no-feature stub preserves the feature-enabled Result API at shared call sites."
)]
pub(crate) fn resize_image_if_needed(
    bytes: &[u8],
    mime_type: &'static str,
) -> Result<ResizedImage> {
    Ok(ResizedImage::original(bytes.to_vec(), mime_type))
}

// ============================================================================
// Tool Registry
// ============================================================================

/// Registry of enabled tools for a Pi run.
///
/// The registry is constructed from configuration (enabled tool names + settings) and is used for:
/// - Looking up a tool implementation by name during tool-call execution.
/// - Enumerating tool schemas when building provider requests.
pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
}

impl ToolRegistry {
    /// Create a new registry with the specified tools enabled.
    pub fn new(enabled: &[&str], cwd: &Path, config: Option<&Config>) -> Self {
        let mut tools: Vec<Box<dyn Tool>> = Vec::new();
        let shell_path = config.and_then(|c| c.shell_path.clone());
        let shell_command_prefix = config.and_then(|c| c.shell_command_prefix.clone());
        let image_auto_resize = config.is_none_or(Config::image_auto_resize);
        let block_images = config
            .and_then(|c| c.images.as_ref().and_then(|i| i.block_images))
            .unwrap_or(false);

        for name in enabled {
            match *name {
                "read" => tools.push(Box::new(ReadTool::with_settings(
                    cwd,
                    image_auto_resize,
                    block_images,
                ))),
                "bash" => tools.push(Box::new(BashTool::with_shell(
                    cwd,
                    shell_path.clone(),
                    shell_command_prefix.clone(),
                ))),
                "edit" => tools.push(Box::new(EditTool::new(cwd))),
                "write" => tools.push(Box::new(WriteTool::new(cwd))),
                "grep" => tools.push(Box::new(GrepTool::new(cwd))),
                "find" => tools.push(Box::new(FindTool::new(cwd))),
                "ls" => tools.push(Box::new(LsTool::new(cwd))),
                "hashline_edit" => tools.push(Box::new(HashlineEditTool::new(cwd))),
                _ => {}
            }
        }

        Self { tools }
    }

    /// Construct a registry from a pre-built tool list.
    pub fn from_tools(tools: Vec<Box<dyn Tool>>) -> Self {
        Self { tools }
    }

    /// Convert the registry into the owned tool list.
    pub fn into_tools(self) -> Vec<Box<dyn Tool>> {
        self.tools
    }

    /// Append a tool.
    pub fn push(&mut self, tool: Box<dyn Tool>) {
        self.tools.push(tool);
    }

    /// Extend the registry with additional tools.
    pub fn extend<I>(&mut self, tools: I)
    where
        I: IntoIterator<Item = Box<dyn Tool>>,
    {
        self.tools.extend(tools);
    }

    /// Get all tools.
    pub fn tools(&self) -> &[Box<dyn Tool>] {
        &self.tools
    }

    /// Find a tool by name.
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|t| t.name() == name)
            .map(std::convert::AsRef::as_ref)
    }
}

// ============================================================================
// Read Tool
// ============================================================================

/// Input parameters for the read tool.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReadInput {
    path: String,
    offset: Option<i64>,
    limit: Option<i64>,
    #[serde(default)]
    hashline: bool,
}

pub struct ReadTool {
    cwd: PathBuf,
    /// Whether to auto-resize images to fit token limits.
    auto_resize: bool,
    block_images: bool,
}

impl ReadTool {
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            auto_resize: true,
            block_images: false,
        }
    }

    pub fn with_settings(cwd: &Path, auto_resize: bool, block_images: bool) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            auto_resize,
            block_images,
        }
    }
}

async fn read_some<R>(reader: &mut R, dst: &mut [u8]) -> std::io::Result<usize>
where
    R: AsyncRead + Unpin,
{
    if dst.is_empty() {
        return Ok(0);
    }

    futures::future::poll_fn(|cx| {
        let mut read_buf = ReadBuf::new(dst);
        match std::pin::Pin::new(&mut *reader).poll_read(cx, &mut read_buf) {
            std::task::Poll::Ready(Ok(())) => std::task::Poll::Ready(Ok(read_buf.filled().len())),
            std::task::Poll::Ready(Err(err)) => std::task::Poll::Ready(Err(err)),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    })
    .await
}

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }
    fn label(&self) -> &str {
        "read"
    }
    fn description(&self) -> &str {
        "Read the contents of a file. Supports text files and images (jpg, png, gif, webp). Images are sent as attachments. For text files, output is truncated to 2000 lines or 1MB (whichever is hit first). Use offset/limit for large files. When you need the full file, continue with offset until complete."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to read (relative or absolute)"
                },
                "offset": {
                    "type": "integer",
                    "description": "Line number to start reading from (1-indexed)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of lines to read"
                },
                "hashline": {
                    "type": "boolean",
                    "description": "When true, output each line as N#AB:content where N is the line number and AB is a content hash. Use with hashline_edit tool for precise edits."
                }
            },
            "required": ["path"]
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    #[allow(clippy::too_many_lines)]
    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let input: ReadInput =
            serde_json::from_value(input).map_err(|e| Error::validation(e.to_string()))?;

        if matches!(input.limit, Some(limit) if limit <= 0) {
            return Err(Error::validation(
                "`limit` must be greater than 0".to_string(),
            ));
        }
        if matches!(input.offset, Some(offset) if offset < 0) {
            return Err(Error::validation(
                "`offset` must be non-negative".to_string(),
            ));
        }

        let path = resolve_read_path(&input.path, &self.cwd);
        let path = enforce_read_scope(&path, &self.cwd)?;

        let meta = asupersync::fs::metadata(&path).await.ok();
        if let Some(meta) = &meta {
            if !meta.is_file() {
                return Err(Error::tool(
                    "read",
                    format!("Path {} is not a regular file", path.display()),
                ));
            }
        }

        let mut file = asupersync::fs::File::open(&path)
            .await
            .map_err(|e| Error::tool("read", e.to_string()))?;

        // Read initial chunk for mime detection
        let mut buffer = [0u8; 8192];
        let mut initial_read = 0;
        loop {
            let n = read_some(&mut file, &mut buffer[initial_read..])
                .await
                .map_err(|e| Error::tool("read", format!("Failed to read file: {e}")))?;
            if n == 0 {
                break;
            }
            initial_read += n;
            if initial_read == buffer.len() {
                break;
            }
        }
        let initial_bytes = &buffer[..initial_read];

        if let Some(mime_type) = detect_supported_image_mime_type_from_bytes(initial_bytes) {
            if self.block_images {
                return Err(Error::tool(
                    "read",
                    "Images are blocked by configuration".to_string(),
                ));
            }

            // For images, allow a larger on-disk source as long as it stays
            // within the read-tool input bound; resize/re-encode may still
            // bring the API payload under IMAGE_MAX_BYTES.
            let max_image_input_bytes = usize::try_from(READ_TOOL_MAX_BYTES).unwrap_or(usize::MAX);
            if let Some(meta) = &meta {
                if meta.len() > READ_TOOL_MAX_BYTES {
                    return Err(Error::tool(
                        "read",
                        format!(
                            "Image is too large ({} bytes). Max allowed is {} bytes.",
                            meta.len(),
                            READ_TOOL_MAX_BYTES
                        ),
                    ));
                }
            }
            let mut all_bytes = Vec::with_capacity(initial_read);
            all_bytes.extend_from_slice(initial_bytes);

            let remaining_limit = max_image_input_bytes.saturating_sub(initial_read);
            let mut limiter = file.take((remaining_limit as u64).saturating_add(1));
            limiter
                .read_to_end(&mut all_bytes)
                .await
                .map_err(|e| Error::tool("read", format!("Failed to read image: {e}")))?;

            if all_bytes.len() > max_image_input_bytes {
                return Err(Error::tool(
                    "read",
                    format!(
                        "Image is too large ({} bytes). Max allowed is {} bytes.",
                        all_bytes.len(),
                        READ_TOOL_MAX_BYTES
                    ),
                ));
            }

            let resized = if self.auto_resize {
                resize_image_if_needed(&all_bytes, mime_type)?
            } else {
                ResizedImage::original(all_bytes, mime_type)
            };

            if resized.bytes.len() > IMAGE_MAX_BYTES {
                let message = if resized.resized {
                    format!(
                        "Image is too large ({} bytes) after resizing. Max allowed is {} bytes.",
                        resized.bytes.len(),
                        IMAGE_MAX_BYTES
                    )
                } else {
                    format!(
                        "Image is too large ({} bytes). Max allowed is {} bytes.",
                        resized.bytes.len(),
                        IMAGE_MAX_BYTES
                    )
                };
                return Err(Error::tool("read", message));
            }

            let base64_data =
                base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &resized.bytes);

            let mut note = format!("Read image file [{}]", resized.mime_type);
            if resized.resized {
                if let (Some(ow), Some(oh), Some(w), Some(h)) = (
                    resized.original_width,
                    resized.original_height,
                    resized.width,
                    resized.height,
                ) {
                    if w > 0 {
                        let scale = f64::from(ow) / f64::from(w);
                        let _ = write!(
                            note,
                            "\n[Image: original {ow}x{oh}, displayed at {w}x{h}. Multiply coordinates by {scale:.2} to map to original image.]"
                        );
                    } else {
                        let _ =
                            write!(note, "\n[Image: original {ow}x{oh}, displayed at {w}x{h}.]");
                    }
                }
            }

            return Ok(ToolOutput {
                content: vec![
                    ContentBlock::Text(TextContent::new(note)),
                    ContentBlock::Image(ImageContent {
                        data: base64_data,
                        mime_type: resized.mime_type.to_string(),
                    }),
                ],
                details: None,
                is_error: false,
            });
        }

        // Text path: optimized streaming read.
        // We need:
        // 1. Total line count.
        // 2. Content for the requested range (offset/limit) OR head/tail if no range.

        // Reset file to start if we read some bytes
        if initial_read > 0 {
            file.seek(SeekFrom::Start(0))
                .await
                .map_err(|e| Error::tool("read", format!("Failed to seek: {e}")))?;
        }

        let mut raw_content = Vec::new();
        let mut newlines_seen = 0usize;

        // Input offset is 1-based. Convert to 0-based index.
        let start_line_idx = match input.offset {
            Some(n) if n > 0 => n.saturating_sub(1).try_into().unwrap_or(usize::MAX),
            _ => 0,
        };
        let limit_lines = input
            .limit
            .map_or(usize::MAX, |l| l.try_into().unwrap_or(usize::MAX));
        let end_line_idx = start_line_idx.saturating_add(limit_lines);

        let mut collecting = start_line_idx == 0;
        let mut buf = vec![0u8; 64 * 1024].into_boxed_slice(); // 64KB chunks
        let mut last_byte_was_newline = false;
        let mut pending_cr = false;

        // We need to track total_lines accurately for the output.
        // We will respect MAX_BYTES for *collected* content, but continue scanning for line counts
        // so pagination metadata is correct.
        let mut total_bytes_read = 0u64;

        loop {
            let n = read_some(&mut file, &mut buf)
                .await
                .map_err(|e| Error::tool("read", e.to_string()))?;
            if n == 0 {
                break;
            }
            total_bytes_read = total_bytes_read.saturating_add(n as u64);

            let chunk = normalize_line_endings_chunk(&buf[..n], &mut pending_cr);
            if chunk.is_empty() {
                continue;
            }
            last_byte_was_newline = chunk.last().is_some_and(|byte| *byte == b'\n');
            let mut chunk_cursor = 0;

            for pos in memchr::memchr_iter(b'\n', &chunk) {
                // Check if this newline marks the end of a line we are collecting
                if collecting {
                    // newlines_seen is the index of the line ending at this newline
                    if newlines_seen + 1 == end_line_idx {
                        // We reached the limit. Collect up to this newline.
                        if raw_content.len() < DEFAULT_MAX_BYTES {
                            let remaining = DEFAULT_MAX_BYTES - raw_content.len();
                            let slice_len = (pos + 1 - chunk_cursor).min(remaining);
                            raw_content
                                .extend_from_slice(&chunk[chunk_cursor..chunk_cursor + slice_len]);
                        }
                        collecting = false;
                        chunk_cursor = pos + 1;
                    }
                }

                newlines_seen += 1;

                // Check if this newline marks the start of the window
                if !collecting && newlines_seen == start_line_idx {
                    collecting = true;
                    chunk_cursor = pos + 1;
                }
            }

            // Append remainder of chunk if collecting
            if collecting && chunk_cursor < chunk.len() && raw_content.len() < DEFAULT_MAX_BYTES {
                let remaining = DEFAULT_MAX_BYTES - raw_content.len();
                let slice_len = (chunk.len() - chunk_cursor).min(remaining);
                raw_content.extend_from_slice(&chunk[chunk_cursor..chunk_cursor + slice_len]);
            }
        }

        if pending_cr {
            last_byte_was_newline = true;
            if collecting && raw_content.len() < DEFAULT_MAX_BYTES {
                raw_content.push(b'\n');
            }
            newlines_seen += 1;
        }

        // A trailing newline terminates the last line rather than starting a new one.
        // Also keep empty files at 0 lines so explicit positive offsets can error correctly.
        let total_lines = if total_bytes_read == 0 {
            0
        } else if last_byte_was_newline {
            newlines_seen
        } else {
            newlines_seen + 1
        };
        let text_content = String::from_utf8_lossy(&raw_content).into_owned();

        // Handle empty file.
        // Offset=0 behaves like "start from beginning", but positive offsets should fail.
        if total_lines == 0 {
            if input.offset.unwrap_or(0) > 0 {
                let offset_display = input.offset.unwrap_or(0);
                return Err(Error::tool(
                    "read",
                    format!(
                        "Offset {offset_display} is beyond end of file ({total_lines} lines total)"
                    ),
                ));
            }
            return Ok(ToolOutput {
                content: vec![ContentBlock::Text(TextContent::new(""))],
                details: None,
                is_error: false,
            });
        }

        // Now we have the content (up to safety limit) in memory, but only for the requested window.
        // `text_content` starts at `start_line_idx`.

        let start_line = start_line_idx;
        let start_line_display = start_line.saturating_add(1);

        if start_line >= total_lines {
            let offset_display = input.offset.unwrap_or(0);
            return Err(Error::tool(
                "read",
                format!(
                    "Offset {offset_display} is beyond end of file ({total_lines} lines total)"
                ),
            ));
        }

        let max_lines_for_truncation = input
            .limit
            .and_then(|l| usize::try_from(l).ok())
            .unwrap_or(DEFAULT_MAX_LINES);
        let display_limit = max_lines_for_truncation.saturating_add(1);

        // We calculate lines to take based on the limit, but since we already filtered
        // during read, we can mostly trust `text_content`, except for `DEFAULT_MAX_BYTES` truncation.

        let lines_to_take = limit_lines.min(display_limit);

        let mut selected_content = String::new();
        let line_iter = text_content.split('\n');

        // Note: we use skip(0) because text_content is already offset
        let effective_iter = if text_content.ends_with('\n') {
            line_iter.take(lines_to_take)
        } else {
            line_iter.take(usize::MAX)
        };

        let max_line_num = start_line.saturating_add(lines_to_take).min(total_lines);
        let line_num_width = max_line_num.to_string().len().max(5);

        for (i, line) in effective_iter.enumerate() {
            if i >= lines_to_take || start_line + i >= total_lines {
                break;
            }
            if i > 0 {
                selected_content.push('\n');
            }
            let line_idx = start_line + i; // 0-indexed
            let line = line.strip_suffix('\r').unwrap_or(line);
            if input.hashline {
                let tag = format_hashline_tag(line_idx, line);
                let _ = write!(selected_content, "{tag}:{line}");
            } else {
                let line_num = line_idx + 1;
                let _ = write!(selected_content, "{line_num:>line_num_width$}→{line}");
            }

            if selected_content.len() > DEFAULT_MAX_BYTES * 2 {
                break;
            }
        }

        let mut truncation = truncate_head(
            selected_content,
            max_lines_for_truncation,
            DEFAULT_MAX_BYTES,
        );
        truncation.total_lines = total_lines;

        let mut output_text = std::mem::take(&mut truncation.content);
        let mut details: Option<serde_json::Value> = None;

        if truncation.first_line_exceeds_limit {
            let first_line = text_content.split('\n').next().unwrap_or("");
            let first_line = first_line.strip_suffix('\r').unwrap_or(first_line);
            let first_line_size = format_size(first_line.len());
            output_text = format!(
                "[Line {start_line_display} is {first_line_size}, exceeds {} limit. Use bash: sed -n '{start_line_display}p' '{}' | head -c {DEFAULT_MAX_BYTES}]",
                format_size(DEFAULT_MAX_BYTES),
                input.path.replace('\'', "'\\''")
            );
            details = Some(serde_json::json!({ "truncation": truncation }));
        } else if truncation.truncated {
            let end_line_display = start_line_display
                .saturating_add(truncation.output_lines)
                .saturating_sub(1);
            let next_offset = end_line_display.saturating_add(1);

            if truncation.truncated_by == Some(TruncatedBy::Lines) {
                let _ = write!(
                    output_text,
                    "\n\n[Showing lines {start_line_display}-{end_line_display} of {total_lines}. Use offset={next_offset} to continue.]"
                );
            } else {
                let _ = write!(
                    output_text,
                    "\n\n[Showing lines {start_line_display}-{end_line_display} of {total_lines} ({} limit). Use offset={next_offset} to continue.]",
                    format_size(DEFAULT_MAX_BYTES)
                );
            }

            details = Some(serde_json::json!({ "truncation": truncation }));
        } else {
            // Calculate how many lines we actually displayed
            let displayed_lines = truncation.output_lines;
            let end_line_display = start_line_display
                .saturating_add(displayed_lines)
                .saturating_sub(1);

            if end_line_display < total_lines {
                let remaining = total_lines.saturating_sub(end_line_display);
                let next_offset = end_line_display.saturating_add(1);
                let _ = write!(
                    output_text,
                    "\n\n[{remaining} more lines in file. Use offset={next_offset} to continue.]"
                );
            }
        }

        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(output_text))],
            details,
            is_error: false,
        })
    }
}

// ============================================================================
// Bash Tool
// ============================================================================

/// Input parameters for the bash tool.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BashInput {
    command: String,
    timeout: Option<u64>,
}

pub struct BashTool {
    cwd: PathBuf,
    shell_path: Option<String>,
    command_prefix: Option<String>,
}

#[derive(Debug, Clone)]
pub struct BashRunResult {
    pub output: String,
    pub exit_code: i32,
    pub cancelled: bool,
    pub truncated: bool,
    pub full_output_path: Option<String>,
    pub truncation: Option<TruncationResult>,
}

#[derive(Debug)]
enum BashPipeFrame {
    Chunk(Vec<u8>),
    Error(String),
}

#[allow(clippy::unnecessary_lazy_evaluations)] // lazy eval needed on unix for signal()
fn exit_status_code(status: std::process::ExitStatus) -> i32 {
    status.code().unwrap_or_else(|| {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt as _;
            status.signal().map_or(-1, |signal| -signal)
        }
        #[cfg(not(unix))]
        {
            -1
        }
    })
}

#[allow(clippy::too_many_lines)]
pub(crate) async fn run_bash_command(
    cwd: &Path,
    shell_path: Option<&str>,
    command_prefix: Option<&str>,
    command: &str,
    timeout_secs: Option<u64>,
    on_update: Option<&(dyn Fn(ToolUpdate) + Send + Sync)>,
) -> Result<BashRunResult> {
    let timeout_secs = match timeout_secs {
        None => Some(DEFAULT_BASH_TIMEOUT_SECS),
        Some(0) => None,
        Some(value) => Some(value),
    };
    let command = command_prefix.filter(|p| !p.trim().is_empty()).map_or_else(
        || command.to_string(),
        |prefix| format!("{prefix}\n{command}"),
    );
    let command = format!("trap 'code=$?; wait; exit $code' EXIT\n{command}");

    if !cwd.exists() {
        return Err(Error::tool(
            "bash",
            format!(
                "Working directory does not exist: {}\nCannot execute bash commands.",
                cwd.display()
            ),
        ));
    }

    let shell = shell_path.unwrap_or_else(|| {
        for path in ["/bin/bash", "/usr/bin/bash", "/usr/local/bin/bash"] {
            if Path::new(path).exists() {
                return path;
            }
        }
        "sh"
    });

    let mut cmd = command_with_default_sigpipe_in_dir(shell, cwd)
        .map_err(|e| Error::tool("bash", format!("Failed to prepare shell: {e}")))?;
    cmd.arg("-c")
        .arg(&command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // Place the shell in its own process group so background children
    // can be killed reliably even if the shell exits first.
    isolate_command_process_group(&mut cmd);

    let mut child = cmd
        .spawn()
        .map_err(|e| Error::tool("bash", format!("Failed to spawn shell: {e}")))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::tool("bash", "Missing stdout".to_string()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| Error::tool("bash", "Missing stderr".to_string()))?;

    // Wrap in ProcessGuard for cleanup (including tree kill)
    let mut guard = ProcessGuard::new(child, ProcessCleanupMode::ProcessGroupTree);

    // We use a bounded channel to provide backpressure. If the child process
    // produces output faster than the async loop can drain it (and spill to disk),
    // the pump threads will block on send(), which stops them from reading from the OS pipe.
    // The OS pipe buffer will fill up, causing the child's `write()` calls to block.
    // This correctly pauses the child until we catch up, preventing unbounded memory growth (OOM).
    let (tx, rx) = mpsc::sync_channel::<BashPipeFrame>(1024);
    let tx_stdout = tx.clone();

    // Design Decision (bd-xdcrh.4.3):
    // We intentionally use raw dedicated OS threads here rather than `asupersync::runtime::spawn_blocking`.
    // The `pump_stream` loop blocks indefinitely on `read()` until the subprocess closes the pipe (EOF).
    // If we used the runtime's blocking pool, concurrently running long-lived bash tools (like compilers
    // or servers) could easily exhaust the pool's thread limit, starving the rest of the application
    // of threads needed for short-lived blocking I/O (e.g., SQLite transactions or filesystem metadata).
    // Dedicated threads cleanly isolate this unbounded blocking risk.
    let stdout_thread = thread::spawn(move || pump_stream(stdout, "stdout", &tx_stdout));
    let stderr_thread = thread::spawn(move || pump_stream(stderr, "stderr", &tx));

    let max_chunks_bytes = DEFAULT_MAX_BYTES.saturating_mul(2);
    let mut bash_output = BashOutputState::new(max_chunks_bytes);
    bash_output.timeout_ms = timeout_secs.map(|s| s.saturating_mul(1000));

    let cx = AgentCx::for_current_or_request();
    let mut timed_out = false;
    let mut cancelled = false;
    let mut exit_code: Option<i32> = None;
    let start = cx
        .cx()
        .timer_driver()
        .map_or_else(wall_now, |timer| timer.now());
    let timeout = timeout_secs.map(Duration::from_secs);
    let mut terminate_deadline: Option<asupersync::Time> = None;

    let tick = Duration::from_millis(10);
    loop {
        let mut updated = false;
        while let Ok(frame) = rx.try_recv() {
            if let Err(err) = ingest_bash_pipe_frame(frame, &mut bash_output).await {
                let _ = guard.kill();
                return Err(err);
            }
            updated = true;
        }

        if updated {
            emit_bash_update(&bash_output, on_update)?;
        }

        match guard.try_wait_child() {
            Ok(Some(status)) => {
                exit_code = Some(exit_status_code(status));
                break;
            }
            Ok(None) => {}
            Err(err) => return Err(Error::tool("bash", err.to_string())),
        }

        let now = cx
            .cx()
            .timer_driver()
            .map_or_else(wall_now, |timer| timer.now());

        if let Some(deadline) = terminate_deadline {
            if now >= deadline {
                if let Some(status) = guard.kill() {
                    exit_code = Some(exit_status_code(status));
                }
                break; // Guard now owns no child after kill()
            }
        } else if let Some(timeout) = timeout {
            let elapsed = std::time::Duration::from_nanos(now.duration_since(start));
            if elapsed >= timeout {
                timed_out = true;
                let pid = guard.child.as_ref().map(std::process::Child::id);
                terminate_process_group_tree(pid);
                terminate_deadline = Some(now + Duration::from_secs(BASH_TERMINATE_GRACE_SECS));
            }
        }

        if terminate_deadline.is_none() && cx.checkpoint().is_err() {
            cancelled = true;
            let _ = guard.kill();
            exit_code = Some(-1);
            break;
        }

        sleep(now, tick).await;
    }

    // Drain any remaining channel frames while waiting for the pump threads
    // to observe EOF and exit. Because the channel is bounded, they may still
    // be blocked on send() until we consume the buffered output after the child
    // closes its pipe ends. The 5-second cap is a safety net for pathological
    // cases (e.g. the child spawned a grandchild that inherited the pipe fd
    // and is still running).
    {
        let drain_start = cx
            .cx()
            .timer_driver()
            .map_or_else(wall_now, |timer| timer.now());
        let drain_deadline = drain_start + Duration::from_secs(5);
        let allow_drain_cancellation = !cancelled && !timed_out && exit_code.is_none();
        loop {
            // Drain everything currently available in the channel.
            let mut got_data = false;
            while let Ok(frame) = rx.try_recv() {
                if let Err(err) = ingest_bash_pipe_frame(frame, &mut bash_output).await {
                    let _ = guard.kill();
                    return Err(err);
                }
                got_data = true;
            }
            if got_data {
                emit_bash_update(&bash_output, on_update)?;
            }

            // If both pump threads have finished, all data is in the channel
            // and we've drained it above, so we're done.
            if stdout_thread.is_finished() && stderr_thread.is_finished() {
                // One final drain in case they sent items between our last
                // try_recv loop and the is_finished check.
                while let Ok(frame) = rx.try_recv() {
                    if let Err(err) = ingest_bash_pipe_frame(frame, &mut bash_output).await {
                        let _ = guard.kill();
                        return Err(err);
                    }
                }
                break;
            }

            let now = cx
                .cx()
                .timer_driver()
                .map_or_else(wall_now, |timer| timer.now());
            if now >= drain_deadline {
                break;
            }
            if allow_drain_cancellation && cx.checkpoint().is_err() {
                cancelled = true;
                break;
            }
            sleep(now, tick).await;
        }
    }

    // Explicitly reap the child process to prevent zombies. try_wait_child()
    // uses WNOHANG which *should* reap the zombie on the first successful
    // return, but calling wait() as a belt-and-suspenders ensures the zombie
    // is cleaned up even if try_wait missed it (observed on macOS when the
    // child is in its own process group).
    if guard.child.is_some() {
        if let Ok(status) = guard.wait() {
            exit_code.get_or_insert_with(|| exit_status_code(status));
        }
    }

    drop(bash_output.temp_file.take());

    let raw_output = concat_chunks(&bash_output.chunks);
    let full_output = String::from_utf8_lossy(&raw_output).into_owned();
    let full_output_last_line_len = full_output.split('\n').next_back().map_or(0, str::len);

    let mut truncation = truncate_tail(full_output, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
    if bash_output.total_bytes > bash_output.chunks_bytes {
        truncation.truncated = true;
        truncation.truncated_by = Some(TruncatedBy::Bytes);
        truncation.total_bytes = bash_output.total_bytes;
        truncation.total_lines = line_count_from_newline_count(
            bash_output.total_bytes,
            bash_output.line_count,
            bash_output.last_byte_was_newline,
        );
    }

    let mut output_text = if truncation.content.is_empty() {
        "(no output)".to_string()
    } else {
        std::mem::take(&mut truncation.content)
    };

    let mut full_output_path = None;
    if truncation.truncated {
        if let Some(path) = bash_output.temp_file_path.as_ref() {
            full_output_path = Some(path.display().to_string());
        }

        let start_line = truncation
            .total_lines
            .saturating_sub(truncation.output_lines)
            .saturating_add(1);
        let end_line = truncation.total_lines;

        let display_path = full_output_path.as_deref().unwrap_or("undefined");
        let file_limit_hit = bash_output.total_bytes > BASH_FILE_LIMIT_BYTES;
        let output_qualifier = if file_limit_hit {
            format!(
                "Partial output (capped at {})",
                format_size(BASH_FILE_LIMIT_BYTES)
            )
        } else {
            "Full output".to_string()
        };

        if truncation.last_line_partial {
            let last_line_size = format_size(full_output_last_line_len);
            let _ = write!(
                output_text,
                "\n\n[Showing last {} of line {end_line} (line is {last_line_size}). {output_qualifier}: {display_path}]",
                format_size(truncation.output_bytes)
            );
        } else if truncation.truncated_by == Some(TruncatedBy::Lines) {
            let _ = write!(
                output_text,
                "\n\n[Showing lines {start_line}-{end_line} of {}. {output_qualifier}: {display_path}]",
                truncation.total_lines
            );
        } else {
            let _ = write!(
                output_text,
                "\n\n[Showing lines {start_line}-{end_line} of {} ({} limit). {output_qualifier}: {display_path}]",
                truncation.total_lines,
                format_size(DEFAULT_MAX_BYTES)
            );
        }
    }

    if timed_out {
        cancelled = true;
        if !output_text.is_empty() {
            output_text.push_str("\n\n");
        }
        let timeout_display = timeout_secs.unwrap_or(0);
        let _ = write!(
            output_text,
            "Command timed out after {timeout_display} seconds"
        );
    }

    let exit_code = exit_code.unwrap_or(-1);
    if !cancelled && exit_code != 0 {
        let _ = write!(output_text, "\n\nCommand exited with code {exit_code}");
    }

    Ok(BashRunResult {
        output: output_text,
        exit_code,
        cancelled,
        truncated: truncation.truncated,
        full_output_path,
        truncation: if truncation.truncated {
            Some(truncation)
        } else {
            None
        },
    })
}

impl BashTool {
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            shell_path: None,
            command_prefix: None,
        }
    }

    pub fn with_shell(
        cwd: &Path,
        shell_path: Option<String>,
        command_prefix: Option<String>,
    ) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            shell_path,
            command_prefix,
        }
    }
}

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }
    fn label(&self) -> &str {
        "bash"
    }
    fn description(&self) -> &str {
        "Execute a bash command in the current working directory. Returns stdout and stderr. Output is truncated to last 2000 lines or 1MB (whichever is hit first). If truncated, full output is saved to a temp file. `timeout` defaults to 120 seconds; set `timeout: 0` to disable."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Bash command to execute"
                },
                "timeout": {
                    "type": "integer",
                    "description": "Timeout in seconds (default 120; set 0 to disable)"
                }
            },
            "required": ["command"]
        })
    }

    #[allow(clippy::too_many_lines)]
    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let input: BashInput =
            serde_json::from_value(input).map_err(|e| Error::validation(e.to_string()))?;

        let result = run_bash_command(
            &self.cwd,
            self.shell_path.as_deref(),
            self.command_prefix.as_deref(),
            &input.command,
            input.timeout,
            on_update.as_deref(),
        )
        .await?;

        let mut details_map = serde_json::Map::new();
        if let Some(truncation) = result.truncation.as_ref() {
            details_map.insert("truncation".to_string(), serde_json::to_value(truncation)?);
        }
        if let Some(path) = result.full_output_path.as_ref() {
            details_map.insert(
                "fullOutputPath".to_string(),
                serde_json::Value::String(path.clone()),
            );
        }

        let details = if details_map.is_empty() {
            None
        } else {
            Some(serde_json::Value::Object(details_map))
        };

        let is_error = result.cancelled || result.exit_code != 0;

        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(result.output))],
            details,
            is_error,
        })
    }
}

// ============================================================================
// Edit Tool
// ============================================================================

/// Input parameters for the edit tool.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EditInput {
    path: String,
    old_text: String,
    new_text: String,
}

pub struct EditTool {
    cwd: PathBuf,
}

impl EditTool {
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
        }
    }
}

fn strip_bom(s: &str) -> (&str, bool) {
    s.strip_prefix('\u{FEFF}')
        .map_or_else(|| (s, false), |stripped| (stripped, true))
}

fn detect_line_ending(content: &str) -> &'static str {
    let bytes = content.as_bytes();
    let mut idx = 0;
    while idx < bytes.len() {
        match bytes[idx] {
            b'\r' => {
                return if bytes.get(idx + 1) == Some(&b'\n') {
                    "\r\n"
                } else {
                    "\r"
                };
            }
            b'\n' => return "\n",
            _ => idx += 1,
        }
    }
    "\n"
}

fn normalize_to_lf(text: &str) -> String {
    if !text.contains('\r') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\r' {
            out.push('\n');
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn normalize_line_endings_chunk<'a>(
    chunk: &'a [u8],
    pending_cr: &mut bool,
) -> std::borrow::Cow<'a, [u8]> {
    if !*pending_cr && memchr::memchr(b'\r', chunk).is_none() {
        return std::borrow::Cow::Borrowed(chunk);
    }

    let mut normalized = Vec::with_capacity(chunk.len().saturating_add(usize::from(*pending_cr)));
    let mut idx = 0;

    if *pending_cr {
        normalized.push(b'\n');
        if chunk.first() == Some(&b'\n') {
            idx = 1;
        }
        *pending_cr = false;
    }

    while idx < chunk.len() {
        match chunk[idx] {
            b'\r' => {
                if chunk.get(idx + 1) == Some(&b'\n') {
                    normalized.push(b'\n');
                    idx += 2;
                } else if idx + 1 < chunk.len() {
                    normalized.push(b'\n');
                    idx += 1;
                } else {
                    *pending_cr = true;
                    idx += 1;
                }
            }
            byte => {
                normalized.push(byte);
                idx += 1;
            }
        }
    }

    std::borrow::Cow::Owned(normalized)
}

fn restore_line_endings(text: &str, ending: &str) -> String {
    match ending {
        "\r\n" => text.replace('\n', "\r\n"),
        "\r" => text.replace('\n', "\r"),
        _ => text.to_string(),
    }
}

#[derive(Debug, Clone)]
struct FuzzyMatchResult {
    found: bool,
    index: usize,
    match_length: usize,
    exact_match: bool,
}

/// Map a range in normalized content back to byte offsets in the original text.
///
/// Returns `(original_start_byte_idx, original_match_byte_len)`.
fn map_normalized_range_to_original(
    content: &str,
    norm_match_start: usize,
    norm_match_len: usize,
) -> (usize, usize) {
    let mut norm_idx = 0;
    let mut orig_idx = 0;
    let mut match_start = None;
    let mut match_end = None;
    let norm_match_end = norm_match_start + norm_match_len;
    let mut last_trimmed_end = 0;
    let mut last_has_newline = false;

    for line in content.split_inclusive('\n') {
        let line_content = line.strip_suffix('\n').unwrap_or(line);
        let has_newline = line.ends_with('\n');
        let trimmed_len = line_content
            .trim_end_matches(|c: char| c.is_whitespace() || is_special_unicode_space(c))
            .len();
        let trimmed_end = orig_idx + trimmed_len;
        last_trimmed_end = trimmed_end;
        last_has_newline = has_newline;

        for (char_offset, c) in line_content.char_indices() {
            // match_end can be detected at any position including trailing
            // whitespace — it correctly points to right after the last content char.
            if norm_idx == norm_match_end && match_end.is_none() {
                match_end = Some(orig_idx + char_offset);
            }

            if char_offset >= trimmed_len {
                continue;
            }

            // match_start must only be detected at non-trailing-whitespace positions.
            // During trailing whitespace, norm_idx is "frozen" at the value after the
            // last real char, which corresponds to the newline in normalized content —
            // not the trailing space. The post-loop newline check handles that case.
            if norm_idx == norm_match_start && match_start.is_none() {
                match_start = Some(orig_idx + char_offset);
            }
            if match_start.is_some() && match_end.is_some() {
                break;
            }

            let normalized_char = if is_special_unicode_space(c) {
                ' '
            } else if matches!(c, '\u{2018}' | '\u{2019}') {
                '\''
            } else if matches!(c, '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}') {
                '"'
            } else if matches!(
                c,
                '\u{2010}'
                    | '\u{2011}'
                    | '\u{2012}'
                    | '\u{2013}'
                    | '\u{2014}'
                    | '\u{2015}'
                    | '\u{2212}'
            ) {
                '-'
            } else {
                c
            };

            norm_idx += normalized_char.len_utf8();
        }

        orig_idx += line_content.len();

        if has_newline {
            if norm_idx == norm_match_start && match_start.is_none() {
                match_start = Some(orig_idx);
            }
            if norm_idx == norm_match_end && match_end.is_none() {
                match_end = Some(trimmed_end);
            }

            norm_idx += 1;
            orig_idx += 1;
        }

        if match_start.is_some() && match_end.is_some() {
            break;
        }
    }

    if norm_idx == norm_match_end && match_end.is_none() {
        match_end = Some(if last_has_newline {
            orig_idx
        } else {
            last_trimmed_end
        });
    }

    let start = match_start.unwrap_or(0);
    let end = match_end.unwrap_or(content.len());
    (start, end.saturating_sub(start))
}

fn build_normalized_content(content: &str) -> String {
    let mut normalized = String::with_capacity(content.len());
    let mut lines = content.split('\n').peekable();

    while let Some(line) = lines.next() {
        let trimmed_len = line
            .trim_end_matches(|c: char| c.is_whitespace() || is_special_unicode_space(c))
            .len();
        for (char_offset, c) in line.char_indices() {
            if char_offset >= trimmed_len {
                continue;
            }
            let normalized_char = if is_special_unicode_space(c) {
                ' '
            } else if matches!(c, '\u{2018}' | '\u{2019}') {
                '\''
            } else if matches!(c, '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}') {
                '"'
            } else if matches!(
                c,
                '\u{2010}'
                    | '\u{2011}'
                    | '\u{2012}'
                    | '\u{2013}'
                    | '\u{2014}'
                    | '\u{2015}'
                    | '\u{2212}'
            ) {
                '-'
            } else {
                c
            };
            normalized.push(normalized_char);
        }
        if lines.peek().is_some() {
            normalized.push('\n');
        }
    }
    normalized
}

fn fuzzy_find_text(content: &str, old_text: &str) -> FuzzyMatchResult {
    fuzzy_find_text_with_normalized(content, old_text, None, None)
}

/// Like [`fuzzy_find_text`], but accepts optional pre-computed normalized
/// versions.
fn fuzzy_find_text_with_normalized(
    content: &str,
    old_text: &str,
    precomputed_content: Option<&str>,
    precomputed_old: Option<&str>,
) -> FuzzyMatchResult {
    use std::borrow::Cow;

    // First, try exact match (fastest path)
    if let Some(index) = content.find(old_text) {
        return FuzzyMatchResult {
            found: true,
            index,
            match_length: old_text.len(),
            exact_match: true,
        };
    }

    // Build normalized versions (reuse pre-computed if available)
    let normalized_content = precomputed_content.map_or_else(
        || Cow::Owned(build_normalized_content(content)),
        Cow::Borrowed,
    );
    let normalized_old_text = precomputed_old.map_or_else(
        || Cow::Owned(build_normalized_content(old_text)),
        Cow::Borrowed,
    );

    // Try to find the normalized old_text in normalized content
    if let Some(normalized_index) = normalized_content.find(normalized_old_text.as_ref()) {
        let (original_start, original_match_len) =
            map_normalized_range_to_original(content, normalized_index, normalized_old_text.len());

        return FuzzyMatchResult {
            found: true,
            index: original_start,
            match_length: original_match_len,
            exact_match: false,
        };
    }

    FuzzyMatchResult {
        found: false,
        index: 0,
        match_length: 0,
        exact_match: false,
    }
}

fn count_overlapping_occurrences(haystack: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return 0;
    }

    haystack
        .char_indices()
        .filter(|(idx, _)| haystack[*idx..].starts_with(needle))
        .count()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiffTag {
    Equal,
    Added,
    Removed,
}

#[derive(Debug, Clone)]
struct DiffPart {
    tag: DiffTag,
    value: String,
}

fn diff_parts(old_content: &str, new_content: &str) -> Vec<DiffPart> {
    use similar::ChangeTag;

    let diff = similar::TextDiff::from_lines(old_content, new_content);

    let mut parts: Vec<DiffPart> = Vec::new();
    let mut current_tag: Option<DiffTag> = None;
    let mut current_lines: Vec<&str> = Vec::new();

    for change in diff.iter_all_changes() {
        let tag = match change.tag() {
            ChangeTag::Equal => DiffTag::Equal,
            ChangeTag::Insert => DiffTag::Added,
            ChangeTag::Delete => DiffTag::Removed,
        };

        let mut line = change.value();
        if let Some(stripped) = line.strip_suffix('\n') {
            line = stripped;
        }

        if current_tag == Some(tag) {
            current_lines.push(line);
        } else {
            if let Some(prev_tag) = current_tag {
                parts.push(DiffPart {
                    tag: prev_tag,
                    value: current_lines.join("\n"),
                });
            }
            current_tag = Some(tag);
            current_lines = vec![line];
        }
    }

    if let Some(tag) = current_tag {
        parts.push(DiffPart {
            tag,
            value: current_lines.join("\n"),
        });
    }

    parts
}

fn diff_line_num_width(old_content: &str, new_content: &str) -> usize {
    // Count newlines with memchr (avoids iterator-item overhead of split().count())
    let old_line_count = memchr::memchr_iter(b'\n', old_content.as_bytes()).count() + 1;
    let new_line_count = memchr::memchr_iter(b'\n', new_content.as_bytes()).count() + 1;
    let max_line_num = old_line_count.max(new_line_count).max(1);
    max_line_num.ilog10() as usize + 1
}

fn split_diff_lines(value: &str) -> Vec<&str> {
    // value is joined by `\n` from a Vec<&str> in diff_parts, so there is no
    // spurious trailing newline. We can split exactly.
    // We only need to handle the case where value is empty but it originated from
    // 0 elements, but `diff_parts` only emits when there is at least 1 line.
    // If value is "", `split('\n')` returns `[""]`, which correctly represents 1 empty line.
    value.split('\n').collect()
}

#[inline]
const fn is_change_tag(tag: DiffTag) -> bool {
    matches!(tag, DiffTag::Added | DiffTag::Removed)
}

#[derive(Debug)]
struct DiffRenderState {
    output: String,
    old_line_num: usize,
    new_line_num: usize,
    last_was_change: bool,
    first_changed_line: Option<usize>,
    line_num_width: usize,
    context_lines: usize,
}

impl DiffRenderState {
    const fn new(line_num_width: usize, context_lines: usize) -> Self {
        Self {
            output: String::new(),
            old_line_num: 1,
            new_line_num: 1,
            last_was_change: false,
            first_changed_line: None,
            line_num_width,
            context_lines,
        }
    }

    #[inline]
    fn ensure_line_break(&mut self) {
        if !self.output.is_empty() {
            self.output.push('\n');
        }
    }

    const fn mark_first_change(&mut self) {
        if self.first_changed_line.is_none() {
            self.first_changed_line = Some(self.new_line_num);
        }
    }

    fn push_added_line(&mut self, line: &str) {
        self.ensure_line_break();
        let _ = write!(
            self.output,
            "+{line_num:>width$} {line}",
            line_num = self.new_line_num,
            width = self.line_num_width
        );
        self.new_line_num = self.new_line_num.saturating_add(1);
    }

    fn push_removed_line(&mut self, line: &str) {
        self.ensure_line_break();
        let _ = write!(
            self.output,
            "-{line_num:>width$} {line}",
            line_num = self.old_line_num,
            width = self.line_num_width
        );
        self.old_line_num = self.old_line_num.saturating_add(1);
    }

    fn push_context_line(&mut self, line: &str) {
        self.ensure_line_break();
        let _ = write!(
            self.output,
            " {line_num:>width$} {line}",
            line_num = self.old_line_num,
            width = self.line_num_width
        );
        self.old_line_num = self.old_line_num.saturating_add(1);
        self.new_line_num = self.new_line_num.saturating_add(1);
    }

    fn push_skip_marker(&mut self, skip: usize) {
        if skip == 0 {
            return;
        }
        self.ensure_line_break();
        let _ = write!(
            self.output,
            " {:>width$} ...",
            " ",
            width = self.line_num_width
        );
        self.old_line_num = self.old_line_num.saturating_add(skip);
        self.new_line_num = self.new_line_num.saturating_add(skip);
    }
}

fn render_changed_part(tag: DiffTag, raw: &[&str], state: &mut DiffRenderState) {
    state.mark_first_change();
    for line in raw {
        match tag {
            DiffTag::Added => state.push_added_line(line),
            DiffTag::Removed => state.push_removed_line(line),
            DiffTag::Equal => {}
        }
    }
    state.last_was_change = true;
}

fn render_equal_part(raw: &[&str], next_part_is_change: bool, state: &mut DiffRenderState) {
    if !(state.last_was_change || next_part_is_change) {
        let raw_len = raw.len();
        state.old_line_num = state.old_line_num.saturating_add(raw_len);
        state.new_line_num = state.new_line_num.saturating_add(raw_len);
        state.last_was_change = false;
        return;
    }

    if state.last_was_change
        && next_part_is_change
        && raw.len() > state.context_lines.saturating_mul(2)
    {
        for line in raw.iter().take(state.context_lines) {
            state.push_context_line(line);
        }

        let skip = raw.len().saturating_sub(state.context_lines * 2);
        state.push_skip_marker(skip);

        for line in raw
            .iter()
            .skip(raw.len().saturating_sub(state.context_lines))
        {
            state.push_context_line(line);
        }
    } else {
        // Compute slice bounds directly instead of cloning Vecs
        let start = if state.last_was_change {
            0
        } else {
            raw.len().saturating_sub(state.context_lines)
        };
        let lines_after_start = raw.len().saturating_sub(start);
        let (end, skip_end) = if !next_part_is_change && lines_after_start > state.context_lines {
            (
                start + state.context_lines,
                lines_after_start - state.context_lines,
            )
        } else {
            (raw.len(), 0)
        };

        state.push_skip_marker(start);
        for line in &raw[start..end] {
            state.push_context_line(line);
        }
        state.push_skip_marker(skip_end);
    }

    state.last_was_change = false;
}

fn generate_diff_string(old_content: &str, new_content: &str) -> (String, Option<usize>) {
    let parts = diff_parts(old_content, new_content);
    let mut state = DiffRenderState::new(diff_line_num_width(old_content, new_content), 4);

    for (i, part) in parts.iter().enumerate() {
        let raw = split_diff_lines(&part.value);
        let next_part_is_change = parts.get(i + 1).is_some_and(|next| is_change_tag(next.tag));

        match part.tag {
            DiffTag::Added | DiffTag::Removed => render_changed_part(part.tag, &raw, &mut state),
            DiffTag::Equal => render_equal_part(&raw, next_part_is_change, &mut state),
        }
    }

    (state.output, state.first_changed_line)
}

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }
    fn label(&self) -> &str {
        "edit"
    }
    fn description(&self) -> &str {
        "Edit a file by replacing text. The oldText must match a unique region; matching is exact but normalizes line endings, Unicode spaces/quotes/dashes, and ignores trailing whitespace."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to edit (relative or absolute)"
                },
                "oldText": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Text to find and replace (must match uniquely; matching normalizes line endings, Unicode spaces/quotes/dashes, and ignores trailing whitespace)"
                },
                "newText": {
                    "type": "string",
                    "description": "New text to replace the old text with"
                }
            },
            "required": ["path", "oldText", "newText"]
        })
    }

    #[allow(clippy::too_many_lines)]
    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let input: EditInput =
            serde_json::from_value(input).map_err(|e| Error::validation(e.to_string()))?;

        if input.new_text.len() > WRITE_TOOL_MAX_BYTES {
            return Err(Error::validation(format!(
                "New text size exceeds maximum allowed ({} > {} bytes)",
                input.new_text.len(),
                WRITE_TOOL_MAX_BYTES
            )));
        }

        let absolute_path = resolve_read_path(&input.path, &self.cwd);
        let absolute_path = enforce_cwd_scope(&absolute_path, &self.cwd, "edit")?;

        let meta = asupersync::fs::metadata(&absolute_path)
            .await
            .map_err(|err| {
                let message = match err.kind() {
                    std::io::ErrorKind::NotFound => format!("File not found: {}", input.path),
                    std::io::ErrorKind::PermissionDenied => {
                        format!("Permission denied: {}", input.path)
                    }
                    _ => format!("Failed to access file {}: {err}", input.path),
                };
                Error::tool("edit", message)
            })?;

        if !meta.is_file() {
            return Err(Error::tool(
                "edit",
                format!("Path {} is not a regular file", absolute_path.display()),
            ));
        }
        if meta.len() > READ_TOOL_MAX_BYTES {
            return Err(Error::tool(
                "edit",
                format!(
                    "File is too large ({} bytes). Max allowed for editing is {} bytes.",
                    meta.len(),
                    READ_TOOL_MAX_BYTES
                ),
            ));
        }

        if let Err(err) = asupersync::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&absolute_path)
            .await
        {
            let message = match err.kind() {
                std::io::ErrorKind::NotFound => format!("File not found: {}", input.path),
                std::io::ErrorKind::PermissionDenied => {
                    format!("Permission denied: {}", input.path)
                }
                _ => format!("Failed to open file for editing: {err}"),
            };
            return Err(Error::tool("edit", message));
        }

        // Read bytes strictly up to the limit to prevent OOM if metadata failed or file grows.
        let file = asupersync::fs::File::open(&absolute_path)
            .await
            .map_err(|e| Error::tool("edit", format!("Failed to open file: {e}")))?;
        let mut raw = Vec::new();
        let mut limiter = file.take(READ_TOOL_MAX_BYTES.saturating_add(1));
        limiter
            .read_to_end(&mut raw)
            .await
            .map_err(|e| Error::tool("edit", format!("Failed to read file: {e}")))?;

        if raw.len() > usize::try_from(READ_TOOL_MAX_BYTES).unwrap_or(usize::MAX) {
            return Err(Error::tool(
                "edit",
                format!("File is too large (> {READ_TOOL_MAX_BYTES} bytes)."),
            ));
        }

        let raw_content = String::from_utf8(raw).map_err(|_| {
            Error::tool(
                "edit",
                "File contains invalid UTF-8 characters and cannot be safely edited as text."
                    .to_string(),
            )
        })?;

        // Strip BOM before matching (LLM won't include invisible BOM in oldText).
        let (content_no_bom, had_bom) = strip_bom(&raw_content);

        let original_ending = detect_line_ending(content_no_bom);
        let normalized_content = normalize_to_lf(content_no_bom);
        let content_for_matching =
            if content_no_bom.contains('\r') && !content_no_bom.contains('\n') {
                std::borrow::Cow::Owned(content_no_bom.replace('\r', "\n"))
            } else {
                std::borrow::Cow::Borrowed(content_no_bom)
            };
        let normalized_old_text = normalize_to_lf(&input.old_text);

        if normalized_old_text.is_empty() {
            return Err(Error::tool(
                "edit",
                "The old text cannot be empty. To prepend text, include the first line's content in oldText and newText.".to_string(),
            ));
        }
        if build_normalized_content(&normalized_old_text).is_empty() {
            return Err(Error::tool(
                "edit",
                "The old text must include at least one non-whitespace character.".to_string(),
            ));
        }

        // Try variants of old_text to handle Unicode normalization differences (NFC vs NFD)
        // and potential input normalization (clipboard, LLM output).
        //
        // Note: normalized_content is already LF-normalized but preserves Unicode form
        // (from String::from_utf8).

        let mut variants = Vec::with_capacity(3);
        variants.push(normalized_old_text.clone());

        let nfc = normalized_old_text.nfc().collect::<String>();
        if nfc != normalized_old_text {
            variants.push(nfc);
        }

        let nfd = normalized_old_text.nfd().collect::<String>();
        if nfd != normalized_old_text {
            variants.push(nfd);
        }

        // Pre-compute normalized versions once and reuse for both matching and
        // occurrence counting (avoids 2x redundant O(n) normalization).
        let precomputed_content = build_normalized_content(content_for_matching.as_ref());

        let mut best_match: Option<(FuzzyMatchResult, String, String)> = None;

        for variant in variants {
            let precomputed_variant = build_normalized_content(&variant);
            let match_result = fuzzy_find_text_with_normalized(
                content_for_matching.as_ref(),
                &variant,
                Some(precomputed_content.as_str()),
                Some(precomputed_variant.as_str()),
            );

            if match_result.found {
                best_match = Some((match_result, precomputed_variant, variant));
                break;
            }
        }

        let Some((match_result, normalized_old_text, matched_variant)) = best_match else {
            return Err(Error::tool(
                "edit",
                format!(
                    "Could not find the exact text in {}. The old text must match exactly including all whitespace and newlines.",
                    input.path
                ),
            ));
        };

        // Count occurrences in the same matching mode to avoid false ambiguity
        // when normalized matching collapses distinct trailing whitespace.
        let occurrences = if match_result.exact_match {
            count_overlapping_occurrences(content_for_matching.as_ref(), &matched_variant)
        } else {
            count_overlapping_occurrences(&precomputed_content, &normalized_old_text)
        };

        if occurrences > 1 {
            return Err(Error::tool(
                "edit",
                format!(
                    "Found {occurrences} occurrences of the text in {}. The text must be unique. Please provide more context to make it unique.",
                    input.path
                ),
            ));
        }

        // Perform replacement in the original coordinate space to preserve
        // line endings and unmatched content exactly.
        let idx = match_result.index;
        let match_len = match_result.match_length;

        // Adapt new_text to match the file's line endings.
        // normalize_to_lf ensures we start from a known state (LF), then
        // restore_line_endings converts LFs to the target ending (e.g. CRLF).
        let adapted_new_text =
            restore_line_endings(&normalize_to_lf(&input.new_text), original_ending);

        let new_len = content_no_bom.len() - match_len + adapted_new_text.len();
        let mut new_content = String::with_capacity(new_len);
        new_content.push_str(&content_no_bom[..idx]);
        new_content.push_str(&adapted_new_text);
        new_content.push_str(&content_no_bom[idx + match_len..]);

        if content_no_bom == new_content {
            return Err(Error::tool(
                "edit",
                format!(
                    "No changes made to {}. The replacement produced identical content. This might indicate an issue with special characters or the text not existing as expected.",
                    input.path
                ),
            ));
        }

        let new_content_for_diff = normalize_to_lf(&new_content);

        // Re-add BOM if present.
        let mut final_content = new_content;
        if had_bom {
            final_content = format!("\u{FEFF}{final_content}");
        }

        // Atomic write (safe improvement vs legacy, behavior-equivalent).
        let absolute_path_clone = absolute_path.clone();
        let final_content_bytes = final_content.into_bytes();
        asupersync::runtime::spawn_blocking_io(move || {
            // Capture original permissions before the file is replaced.
            let original_perms = std::fs::metadata(&absolute_path_clone)
                .ok()
                .map(|m| m.permissions());
            let parent = absolute_path_clone
                .parent()
                .unwrap_or_else(|| Path::new("."));
            let mut temp_file = tempfile::NamedTempFile::new_in(parent)?;

            temp_file.as_file_mut().write_all(&final_content_bytes)?;
            temp_file.as_file_mut().sync_all()?;

            // Restore original file permissions (tempfile defaults to 0o600) before persisting.
            if let Some(perms) = original_perms {
                let _ = temp_file.as_file().set_permissions(perms);
            } else {
                // Default to 0644 (rw-r--r--) instead of tempfile's 0600 if we couldn't read original perms.
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = temp_file
                        .as_file()
                        .set_permissions(std::fs::Permissions::from_mode(0o644));
                }
            }

            temp_file
                .persist(&absolute_path_clone)
                .map_err(|e| e.error)?;
            sync_parent_dir(&absolute_path_clone)?;
            Ok(())
        })
        .await
        .map_err(|e| Error::tool("edit", format!("Failed to write file: {e}")))?;

        let (diff, first_changed_line) =
            generate_diff_string(&normalized_content, &new_content_for_diff);
        let mut details = serde_json::Map::new();
        details.insert("diff".to_string(), serde_json::Value::String(diff));
        if let Some(line) = first_changed_line {
            details.insert(
                "firstChangedLine".to_string(),
                serde_json::Value::Number(serde_json::Number::from(line)),
            );
        }

        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(format!(
                "Successfully replaced text in {}.",
                input.path
            )))],
            details: Some(serde_json::Value::Object(details)),
            is_error: false,
        })
    }
}

// ============================================================================
// Write Tool
// ============================================================================

/// Input parameters for the write tool.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WriteInput {
    path: String,
    content: String,
}

pub struct WriteTool {
    cwd: PathBuf,
}

impl WriteTool {
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
        }
    }
}

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }
    fn label(&self) -> &str {
        "write"
    }
    fn description(&self) -> &str {
        "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to write (relative or absolute)"
                },
                "content": {
                    "type": "string",
                    "description": "Content to write to the file"
                }
            },
            "required": ["path", "content"]
        })
    }

    #[allow(clippy::too_many_lines)]
    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let input: WriteInput =
            serde_json::from_value(input).map_err(|e| Error::validation(e.to_string()))?;

        if input.content.len() > WRITE_TOOL_MAX_BYTES {
            return Err(Error::validation(format!(
                "Content size exceeds maximum allowed ({} > {} bytes)",
                input.content.len(),
                WRITE_TOOL_MAX_BYTES
            )));
        }

        let path = resolve_path(&input.path, &self.cwd);
        let path = enforce_cwd_scope(&path, &self.cwd, "write")?;

        if let Ok(meta) = asupersync::fs::metadata(&path).await {
            if !meta.is_file() {
                return Err(Error::tool(
                    "write",
                    format!("Path {} is not a regular file", path.display()),
                ));
            }
            if let Err(err) = asupersync::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .await
            {
                let message = match err.kind() {
                    std::io::ErrorKind::PermissionDenied => {
                        format!("Permission denied: {}", input.path)
                    }
                    _ => format!("Failed to open file for writing: {err}"),
                };
                return Err(Error::tool("write", message));
            }
        }

        // Create parent directories if needed
        if let Some(parent) = path.parent() {
            asupersync::fs::create_dir_all(parent)
                .await
                .map_err(|e| Error::tool("write", format!("Failed to create directories: {e}")))?;
        }

        // Parity with legacy pi-mono: report JS string length (UTF-16 code units) as "bytes".
        let bytes_written = input.content.encode_utf16().count();

        // Write atomically using tempfile on a blocking thread
        let path_clone = path.clone();
        let content_bytes = input.content.into_bytes();
        asupersync::runtime::spawn_blocking_io(move || {
            // Capture original permissions before the file is replaced (new files get None).
            let original_perms = std::fs::metadata(&path_clone).ok().map(|m| m.permissions());
            let parent = path_clone.parent().unwrap_or_else(|| Path::new("."));
            let mut temp_file = tempfile::NamedTempFile::new_in(parent)?;

            temp_file.as_file_mut().write_all(&content_bytes)?;
            temp_file.as_file_mut().sync_all()?;

            // Restore original file permissions (tempfile defaults to 0o600) before persisting.
            if let Some(perms) = original_perms {
                let _ = temp_file.as_file().set_permissions(perms);
            } else {
                // New file: default to 0644 (rw-r--r--) instead of tempfile's 0600.
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = temp_file
                        .as_file()
                        .set_permissions(std::fs::Permissions::from_mode(0o644));
                }
            }

            // Persist (atomic rename)
            temp_file.persist(&path_clone).map_err(|e| e.error)?;
            sync_parent_dir(&path_clone)?;
            Ok(())
        })
        .await
        .map_err(|e| Error::tool("write", format!("Failed to write file: {e}")))?;

        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(format!(
                "Successfully wrote {} bytes to {}",
                bytes_written, input.path
            )))],
            details: None,
            is_error: false,
        })
    }
}

// ============================================================================
// Grep Tool
// ============================================================================

/// Input parameters for the grep tool.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GrepInput {
    pattern: String,
    path: Option<String>,
    glob: Option<String>,
    ignore_case: Option<bool>,
    literal: Option<bool>,
    context: Option<usize>,
    limit: Option<usize>,
    #[serde(default)]
    hashline: bool,
}

pub struct GrepTool {
    cwd: PathBuf,
}

impl GrepTool {
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
        }
    }
}

/// Result of truncating a single grep output line.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TruncateLineResult {
    text: String,
    was_truncated: bool,
}

/// Truncate a single line to max characters, adding a marker suffix.
///
/// Matches pi-mono behavior: `${line.slice(0, maxChars)}... [truncated]`.
fn truncate_line(line: &str, max_chars: usize) -> TruncateLineResult {
    let mut chars = line.chars();
    let prefix: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_none() {
        return TruncateLineResult {
            text: line.to_string(),
            was_truncated: false,
        };
    }

    TruncateLineResult {
        text: format!("{prefix}... [truncated]"),
        was_truncated: true,
    }
}

fn process_rg_json_match_line(
    line_res: std::io::Result<String>,
    matches: &mut Vec<(PathBuf, usize)>,
    match_count: &mut usize,
    match_limit_reached: &mut bool,
    scan_limit: usize,
) {
    if *match_limit_reached {
        return;
    }

    let line = match line_res {
        Ok(l) => l,
        Err(e) => {
            tracing::debug!("Skipping ripgrep output line due to read error: {e}");
            return;
        }
    };
    if line.trim().is_empty() {
        return;
    }

    let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) else {
        return;
    };

    if event.get("type").and_then(serde_json::Value::as_str) != Some("match") {
        return;
    }

    let file_path = event
        .pointer("/data/path/text")
        .and_then(serde_json::Value::as_str)
        .map(PathBuf::from);
    let line_number = event
        .pointer("/data/line_number")
        .and_then(serde_json::Value::as_u64)
        .and_then(|n| usize::try_from(n).ok());

    if let (Some(fp), Some(ln)) = (file_path, line_number) {
        matches.push((fp, ln));
        *match_count += 1;
        if *match_count >= scan_limit {
            *match_limit_reached = true;
        }
    }
}

fn drain_rg_stdout(
    stdout_rx: &std::sync::mpsc::Receiver<std::io::Result<String>>,
    matches: &mut Vec<(PathBuf, usize)>,
    match_count: &mut usize,
    match_limit_reached: &mut bool,
    scan_limit: usize,
) {
    while let Ok(line_res) = stdout_rx.try_recv() {
        process_rg_json_match_line(
            line_res,
            matches,
            match_count,
            match_limit_reached,
            scan_limit,
        );
        if *match_limit_reached {
            break;
        }
    }
}

fn drain_rg_stderr(
    stderr_rx: &std::sync::mpsc::Receiver<std::result::Result<Vec<u8>, String>>,
    stderr_bytes: &mut Vec<u8>,
) -> Result<()> {
    while let Ok(chunk_result) = stderr_rx.try_recv() {
        let chunk = chunk_result
            .map_err(|err| Error::tool("grep", format!("Failed to read stderr: {err}")))?;
        stderr_bytes.extend_from_slice(&chunk);
    }
    Ok(())
}

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }
    fn label(&self) -> &str {
        "grep"
    }
    fn description(&self) -> &str {
        "Search file contents for a pattern. Returns matching lines with file paths and line numbers. Respects .gitignore. Output is truncated to 100 matches or 1MB (whichever is hit first). Long lines are truncated to 500 chars. Use hashline=true to get N#AB content-hash tags for use with hashline_edit."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Search pattern (regex or literal string)"
                },
                "path": {
                    "type": "string",
                    "description": "Directory or file to search (default: current directory)"
                },
                "glob": {
                    "type": "string",
                    "description": "Filter files by glob pattern, e.g. '*.ts' or '**/*.spec.ts'"
                },
                "ignoreCase": {
                    "type": "boolean",
                    "description": "Case-insensitive search (default: false)"
                },
                "literal": {
                    "type": "boolean",
                    "description": "Treat pattern as literal string instead of regex (default: false)"
                },
                "context": {
                    "type": "integer",
                    "description": "Number of lines to show before and after each match (default: 0)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of matches to return (default: 100)"
                },
                "hashline": {
                    "type": "boolean",
                    "description": "When true, output each line as N#AB:content where N is the line number and AB is a content hash. Use with hashline_edit tool for precise edits."
                }
            },
            "required": ["pattern"]
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    #[allow(clippy::too_many_lines)]
    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let input: GrepInput =
            serde_json::from_value(input).map_err(|e| Error::validation(e.to_string()))?;

        if matches!(input.limit, Some(0)) {
            return Err(Error::validation(
                "`limit` must be greater than 0".to_string(),
            ));
        }

        if !rg_available() {
            return Err(Error::tool(
                "grep",
                "ripgrep (rg) is not available (please install ripgrep)".to_string(),
            ));
        }

        let search_dir = input.path.as_deref().unwrap_or(".");
        let search_path = resolve_read_path(search_dir, &self.cwd);
        let search_path = enforce_cwd_scope(&search_path, &self.cwd, "grep")?;

        let is_directory = asupersync::fs::metadata(&search_path)
            .await
            .map_err(|e| {
                Error::tool(
                    "grep",
                    format!("Cannot access path {}: {e}", search_path.display()),
                )
            })?
            .is_dir();

        let context_value = input.context.unwrap_or(0);
        let effective_limit = input.limit.unwrap_or(DEFAULT_GREP_LIMIT).max(1);
        // Overfetch one match so limit notices only appear after confirmed overflow.
        let scan_limit = effective_limit.saturating_add(1);

        let mut args: Vec<String> = vec![
            "--json".to_string(),
            "--line-number".to_string(),
            "--color=never".to_string(),
            "--hidden".to_string(),
            // Prevent massive JSON lines from minified files causing OOM
            "--max-columns=10000".to_string(),
        ];

        if input.ignore_case.unwrap_or(false) {
            args.push("--ignore-case".to_string());
        }
        if input.literal.unwrap_or(false) {
            args.push("--fixed-strings".to_string());
        }
        if let Some(glob) = &input.glob {
            args.push("--glob".to_string());
            args.push(glob.clone());
        }

        // Mirror find-tool behavior: explicitly pass root/nested .gitignore files
        // so ignore rules apply consistently even outside a git worktree.
        let ignore_root = if is_directory {
            search_path.clone()
        } else {
            search_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf()
        };
        // NOTE: We rely on rg's native .gitignore discovery. We only explicitly pass
        // the root .gitignore if it exists, to ensure it's respected even if the
        // search path logic might otherwise miss it (e.g. searching a subdir).
        // We do NOT perform a blocking `glob("**/.gitignore")` here, as that stalls
        // the async runtime on large repos.
        let workspace_gitignore = self.cwd.join(".gitignore");
        if workspace_gitignore.exists() {
            args.push("--ignore-file".to_string());
            args.push(workspace_gitignore.display().to_string());
        }
        let root_gitignore = ignore_root.join(".gitignore");
        if root_gitignore != workspace_gitignore && root_gitignore.exists() {
            args.push("--ignore-file".to_string());
            args.push(root_gitignore.display().to_string());
        }

        args.push("--".to_string());
        args.push(input.pattern.clone());
        args.push(search_path.display().to_string());

        let rg_cmd = find_rg_binary().ok_or_else(|| {
            Error::tool(
                "grep",
                "rg is not available (please install ripgrep or rg)".to_string(),
            )
        })?;

        let mut child = command_with_default_sigpipe(rg_cmd)
            .map_err(|e| Error::tool("grep", format!("Failed to prepare ripgrep: {e}")))?
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| Error::tool("grep", format!("Failed to run ripgrep: {e}")))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::tool("grep", "Missing stdout".to_string()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| Error::tool("grep", "Missing stderr".to_string()))?;

        let mut guard = ProcessGuard::new(child, ProcessCleanupMode::ChildOnly);

        let (stdout_tx, stdout_rx) = std::sync::mpsc::sync_channel(1024);
        let (stderr_tx, stderr_rx) =
            std::sync::mpsc::sync_channel::<std::result::Result<Vec<u8>, String>>(1024);

        let stdout_thread = std::thread::spawn(move || {
            let reader = std::io::BufReader::new(stdout);
            for line in reader.lines() {
                if stdout_tx.send(line).is_err() {
                    break;
                }
            }
        });

        let stderr_thread = std::thread::spawn(move || {
            let reader = std::io::BufReader::new(stderr);
            let _ = stderr_tx.send(read_to_end_capped_and_drain(reader, READ_TOOL_MAX_BYTES));
        });

        let mut matches: Vec<(PathBuf, usize)> = Vec::new();
        let mut match_count: usize = 0;
        let mut match_scan_limit_reached = false;
        let mut stderr_bytes = Vec::new();

        let tick = Duration::from_millis(10);
        let mut cx_cancelled = false;

        let exit_status = loop {
            let agent_cx = AgentCx::for_current_or_request();
            let cx = agent_cx.cx();
            if cx.checkpoint().is_err() {
                cx_cancelled = true;
                break None;
            }

            drain_rg_stdout(
                &stdout_rx,
                &mut matches,
                &mut match_count,
                &mut match_scan_limit_reached,
                scan_limit,
            );
            drain_rg_stderr(&stderr_rx, &mut stderr_bytes)?;

            if match_scan_limit_reached {
                break None;
            }

            match guard.try_wait_child() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => {
                    let now = cx.timer_driver().map_or_else(wall_now, |timer| timer.now());
                    sleep(now, tick).await;
                }
                Err(e) => return Err(Error::tool("grep", e.to_string())),
            }
        };

        drain_rg_stdout(
            &stdout_rx,
            &mut matches,
            &mut match_count,
            &mut match_scan_limit_reached,
            scan_limit,
        );

        let code = if match_scan_limit_reached || cx_cancelled {
            // Avoid buffering unbounded stdout/stderr once we've hit the match limit.
            // `kill()` terminates the process, and we reap it in a background thread
            // so the stdout reader threads can exit promptly without blocking this task.
            let _ = guard.kill();
            // Drop any buffered stdout/stderr lines that were queued before termination.
            while stdout_rx.try_recv().is_ok() {}
            while stderr_rx.try_recv().is_ok() {}
            0
        } else {
            let status = exit_status.expect("rg exit status");
            status.code().unwrap_or(0)
        };

        // Keep draining while waiting for reader threads to finish; otherwise a
        // bounded channel can fill and block the sender thread, causing join()
        // to hang after ripgrep has already exited.
        while !stdout_thread.is_finished() || !stderr_thread.is_finished() {
            if match_scan_limit_reached || cx_cancelled {
                while stdout_rx.try_recv().is_ok() {}
            } else {
                drain_rg_stdout(
                    &stdout_rx,
                    &mut matches,
                    &mut match_count,
                    &mut match_scan_limit_reached,
                    scan_limit,
                );
            }
            drain_rg_stderr(&stderr_rx, &mut stderr_bytes)?;
            sleep(wall_now(), Duration::from_millis(1)).await;
        }

        if cx_cancelled {
            return Err(Error::tool("grep", "Command cancelled"));
        }

        // Ensure stdout/stderr reader threads have fully drained the pipes before
        // we decide whether matches were found. Without this, fast ripgrep runs can
        // exit before the reader thread has delivered JSON match lines, causing
        // false "No matches found" results.
        stdout_thread
            .join()
            .map_err(|_| Error::tool("grep", "ripgrep stdout reader thread panicked"))?;
        stderr_thread
            .join()
            .map_err(|_| Error::tool("grep", "ripgrep stderr reader thread panicked"))?;

        // Drain any remaining stdout/stderr produced after the last poll.
        if match_scan_limit_reached {
            while stdout_rx.try_recv().is_ok() {}
        } else {
            drain_rg_stdout(
                &stdout_rx,
                &mut matches,
                &mut match_count,
                &mut match_scan_limit_reached,
                scan_limit,
            );
        }
        drain_rg_stderr(&stderr_rx, &mut stderr_bytes)?;

        let mut stderr_text = String::from_utf8_lossy(&stderr_bytes).trim().to_string();
        if stderr_bytes.len() as u64 > READ_TOOL_MAX_BYTES {
            stderr_text.push_str("\n... [stderr truncated] ...");
        }
        if !match_scan_limit_reached && code != 0 && code != 1 {
            let msg = if stderr_text.is_empty() {
                format!("ripgrep exited with code {code}")
            } else {
                stderr_text
            };
            return Err(Error::tool("grep", msg));
        }

        let match_limit_reached = match_count > effective_limit;
        if match_limit_reached {
            matches.truncate(effective_limit);
            match_count = effective_limit;
        }

        if match_count == 0 {
            return Ok(ToolOutput {
                content: vec![ContentBlock::Text(TextContent::new("No matches found"))],
                details: None,
                is_error: false,
            });
        }

        let mut file_cache: HashMap<PathBuf, Vec<String>> = HashMap::new();
        let mut output_lines: Vec<String> = Vec::new();
        let mut lines_truncated = false;

        // Group matches by file to merge overlapping context windows
        let mut file_order: Vec<PathBuf> = Vec::new();
        let mut matches_by_file: HashMap<PathBuf, Vec<usize>> = HashMap::new();
        for (file_path, line_number) in &matches {
            if !matches_by_file.contains_key(file_path) {
                file_order.push(file_path.clone());
            }
            matches_by_file
                .entry(file_path.clone())
                .or_default()
                .push(*line_number);
        }

        for file_path in file_order {
            let Some(mut match_lines) = matches_by_file.remove(&file_path) else {
                continue;
            };
            let relative_path = format_grep_path(&file_path, &self.cwd);
            let lines = get_file_lines_async(&file_path, &mut file_cache).await;

            if lines.is_empty() {
                if let Some(first_match) = match_lines.first() {
                    output_lines.push(format!(
                        "{relative_path}:{first_match}: (unable to read file or too large)"
                    ));
                }
                continue;
            }

            match_lines.sort_unstable();
            match_lines.dedup();

            let mut blocks: Vec<(usize, usize)> = Vec::new();
            for &line_number in &match_lines {
                let start = if context_value > 0 {
                    line_number.saturating_sub(context_value).max(1)
                } else {
                    line_number
                };
                let end = if context_value > 0 {
                    line_number.saturating_add(context_value).min(lines.len())
                } else {
                    line_number
                };

                if let Some(last_block) = blocks.last_mut() {
                    if start <= last_block.1.saturating_add(1) {
                        last_block.1 = last_block.1.max(end);
                        continue;
                    }
                }
                blocks.push((start, end));
            }

            for (i, (start, end)) in blocks.into_iter().enumerate() {
                if i > 0 {
                    output_lines.push("--".to_string());
                }
                for current in start..=end {
                    let line_text = lines.get(current - 1).map_or("", String::as_str);
                    let sanitized = line_text.replace('\r', "");
                    let truncated = truncate_line(&sanitized, GREP_MAX_LINE_LENGTH);
                    if truncated.was_truncated {
                        lines_truncated = true;
                    }

                    if input.hashline {
                        let line_idx = current - 1; // 0-indexed for hashline
                        let tag = format_hashline_tag(line_idx, &sanitized);
                        if match_lines.binary_search(&current).is_ok() {
                            output_lines.push(format!("{relative_path}:{tag}: {}", truncated.text));
                        } else {
                            output_lines.push(format!("{relative_path}-{tag}- {}", truncated.text));
                        }
                    } else if match_lines.binary_search(&current).is_ok() {
                        output_lines.push(format!("{relative_path}:{current}: {}", truncated.text));
                    } else {
                        output_lines.push(format!("{relative_path}-{current}- {}", truncated.text));
                    }
                }
            }
        }

        // Apply byte truncation (no line limit since we already have match limit).
        let raw_output = output_lines.join("\n");
        let mut truncation = truncate_head(raw_output, usize::MAX, DEFAULT_MAX_BYTES);

        let mut output = std::mem::take(&mut truncation.content);
        let mut notices: Vec<String> = Vec::new();
        let mut details_map = serde_json::Map::new();

        if match_limit_reached {
            notices.push(format!(
                "{effective_limit} matches limit reached. Use limit={} for more, or refine pattern",
                effective_limit * 2
            ));
            details_map.insert(
                "matchLimitReached".to_string(),
                serde_json::Value::Number(serde_json::Number::from(effective_limit)),
            );
        }

        if truncation.truncated {
            notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
            details_map.insert("truncation".to_string(), serde_json::to_value(truncation)?);
        }

        if lines_truncated {
            notices.push(format!(
                "Some lines truncated to {GREP_MAX_LINE_LENGTH} chars. Use read tool to see full lines"
            ));
            details_map.insert("linesTruncated".to_string(), serde_json::Value::Bool(true));
        }

        if !notices.is_empty() {
            let _ = write!(output, "\n\n[{}]", notices.join(". "));
        }

        let details = if details_map.is_empty() {
            None
        } else {
            Some(serde_json::Value::Object(details_map))
        };

        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(output))],
            details,
            is_error: false,
        })
    }
}

// ============================================================================
// Find Tool
// ============================================================================

/// Input parameters for the find tool.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FindInput {
    pattern: String,
    path: Option<String>,
    limit: Option<usize>,
}

#[derive(Debug)]
struct FindEntry {
    rel: String,
    modified: Option<SystemTime>,
}

pub struct FindTool {
    cwd: PathBuf,
}

impl FindTool {
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
        }
    }
}

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Tool for FindTool {
    fn name(&self) -> &str {
        "find"
    }
    fn label(&self) -> &str {
        "find"
    }
    fn description(&self) -> &str {
        "Search for files by glob pattern. Returns matching file paths relative to the search directory. Sorted by modification time (newest first). Respects .gitignore. Output is truncated to 1000 results or 1MB (whichever is hit first)."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Glob pattern to match files, e.g. '*.ts', '**/*.json', or 'src/**/*.spec.ts'"
                },
                "path": {
                    "type": "string",
                    "description": "Directory to search in (default: current directory)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of results (default: 1000)"
                }
            },
            "required": ["pattern"]
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    #[allow(clippy::too_many_lines)]
    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let input: FindInput =
            serde_json::from_value(input).map_err(|e| Error::validation(e.to_string()))?;

        if matches!(input.limit, Some(0)) {
            return Err(Error::validation(
                "`limit` must be greater than 0".to_string(),
            ));
        }

        let search_dir = input.path.as_deref().unwrap_or(".");
        let search_path = resolve_read_path(search_dir, &self.cwd);
        let search_path = enforce_cwd_scope(&search_path, &self.cwd, "find")?;
        let search_path = strip_unc_prefix(search_path);
        let effective_limit = input.limit.unwrap_or(DEFAULT_FIND_LIMIT);
        // Overfetch one result so limit notices only appear after confirmed overflow.
        let scan_limit = effective_limit.saturating_add(1);

        if !search_path.exists() {
            return Err(Error::tool(
                "find",
                format!("Path not found: {}", search_path.display()),
            ));
        }

        let fd_cmd = find_fd_binary().ok_or_else(|| {
            Error::tool(
                "find",
                "fd is not available (please install fd-find or fd)".to_string(),
            )
        })?;

        // Build fd arguments
        let mut args: Vec<String> = vec![
            "--glob".to_string(),
            "--color=never".to_string(),
            "--hidden".to_string(),
            "--max-results".to_string(),
            scan_limit.to_string(),
        ];

        // NOTE: We rely on fd's native .gitignore discovery. We only explicitly pass
        // the root .gitignore if it exists, to ensure it's respected even if the
        // search path logic might otherwise miss it.
        // We do NOT perform a blocking `glob("**/.gitignore")` here.
        let workspace_gitignore = self.cwd.join(".gitignore");
        if workspace_gitignore.exists() {
            args.push("--ignore-file".to_string());
            args.push(workspace_gitignore.display().to_string());
        }
        let root_gitignore = search_path.join(".gitignore");
        if root_gitignore != workspace_gitignore && root_gitignore.exists() {
            args.push("--ignore-file".to_string());
            args.push(root_gitignore.display().to_string());
        }

        args.push("--".to_string());
        args.push(input.pattern.clone());
        args.push(search_path.display().to_string());

        let mut child = command_with_default_sigpipe_in_dir(fd_cmd, &self.cwd)
            .map_err(|e| Error::tool("find", format!("Failed to prepare fd: {e}")))?
            .args(args)
            .current_dir(&self.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| Error::tool("find", format!("Failed to run fd: {e}")))?;

        let stdout_pipe = child
            .stdout
            .take()
            .ok_or_else(|| Error::tool("find", "Missing stdout"))?;
        let stderr_pipe = child
            .stderr
            .take()
            .ok_or_else(|| Error::tool("find", "Missing stderr"))?;

        let mut guard = ProcessGuard::new(child, ProcessCleanupMode::ChildOnly);

        let stdout_handle = std::thread::spawn(move || -> std::result::Result<Vec<u8>, String> {
            read_to_end_capped_and_drain(stdout_pipe, READ_TOOL_MAX_BYTES)
        });

        let stderr_handle = std::thread::spawn(move || -> std::result::Result<Vec<u8>, String> {
            read_to_end_capped_and_drain(stderr_pipe, READ_TOOL_MAX_BYTES)
        });

        let tick = Duration::from_millis(10);
        let start_time = std::time::Instant::now();
        let timeout_ms = 60_000; // 60 seconds
        let mut timed_out = false;
        let mut cx_cancelled = false;

        let status = loop {
            let agent_cx = AgentCx::for_current_or_request();
            let cx = agent_cx.cx();
            if cx.checkpoint().is_err() {
                cx_cancelled = true;
                let _ = guard.kill();
                break None;
            }

            // Check if process is done
            match guard.try_wait_child() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => {
                    if start_time.elapsed().as_millis() > timeout_ms {
                        timed_out = true;
                        let _ = guard.kill();
                        break None;
                    }
                    let now = cx.timer_driver().map_or_else(wall_now, |timer| timer.now());
                    sleep(now, tick).await;
                }
                Err(e) => return Err(Error::tool("find", e.to_string())),
            }
        };

        let stdout_bytes = stdout_handle
            .join()
            .map_err(|_| Error::tool("find", "fd stdout reader thread panicked"))?
            .map_err(|err| Error::tool("find", format!("Failed to read fd stdout: {err}")))?;
        let stderr_bytes = stderr_handle
            .join()
            .map_err(|_| Error::tool("find", "fd stderr reader thread panicked"))?
            .map_err(|err| Error::tool("find", format!("Failed to read fd stderr: {err}")))?;

        if cx_cancelled {
            return Err(Error::tool("find", "Command cancelled"));
        }
        if timed_out {
            return Err(Error::tool("find", "Command timed out after 60 seconds"));
        }
        let status = status.expect("fd exit status after successful completion");

        let mut stdout = String::from_utf8_lossy(&stdout_bytes).trim().to_string();
        if stdout_bytes.len() as u64 > READ_TOOL_MAX_BYTES {
            stdout.push_str("\n... [stdout truncated] ...");
        }
        let mut stderr = String::from_utf8_lossy(&stderr_bytes).trim().to_string();
        if stderr_bytes.len() as u64 > READ_TOOL_MAX_BYTES {
            stderr.push_str("\n... [stderr truncated] ...");
        }

        if !status.success() && stdout.is_empty() {
            if status.code() == Some(1) && stderr.is_empty() {
                // fd uses exit code 1 for "no matches"; treat as empty result.
            } else {
                let code = status.code().unwrap_or(1);
                let msg = if stderr.is_empty() {
                    format!("fd exited with code {code}")
                } else {
                    stderr
                };
                return Err(Error::tool("find", msg));
            }
        }

        if stdout.is_empty() {
            return Ok(ToolOutput {
                content: vec![ContentBlock::Text(TextContent::new(
                    "No files found matching pattern",
                ))],
                details: None,
                is_error: false,
            });
        }

        let mut entries: Vec<FindEntry> = Vec::new();
        for raw_line in stdout.lines() {
            let line = raw_line.trim_end_matches('\r').trim();
            if line.is_empty() {
                continue;
            }

            // On Windows, fd may emit `//?/…` or `\\?\…` extended-length
            // paths. Strip the prefix so relativization works correctly.
            let clean = strip_unc_prefix(PathBuf::from(line));
            let line_path = clean.as_path();
            let mut rel = if line_path.is_absolute() {
                line_path.strip_prefix(&search_path).map_or_else(
                    |_| line_path.to_string_lossy().to_string(),
                    |stripped| stripped.to_string_lossy().to_string(),
                )
            } else {
                line_path.to_string_lossy().to_string()
            };

            let full_path = if line_path.is_absolute() {
                line_path.to_path_buf()
            } else {
                search_path.join(line_path)
            };
            if full_path.is_dir() && !rel.ends_with('/') {
                rel.push('/');
            }

            let modified = std::fs::metadata(&full_path)
                .and_then(|meta| meta.modified())
                .ok();
            entries.push(FindEntry { rel, modified });
        }

        entries.sort_by(|a, b| {
            let ordering = match (&a.modified, &b.modified) {
                (Some(a_time), Some(b_time)) => b_time.cmp(a_time),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => Ordering::Equal,
            };
            ordering.then_with(|| {
                let a_lower = a.rel.to_lowercase();
                let b_lower = b.rel.to_lowercase();
                a_lower.cmp(&b_lower).then_with(|| a.rel.cmp(&b.rel))
            })
        });

        let mut relativized: Vec<String> = entries.into_iter().map(|entry| entry.rel).collect();

        if relativized.is_empty() {
            return Ok(ToolOutput {
                content: vec![ContentBlock::Text(TextContent::new(
                    "No files found matching pattern",
                ))],
                details: None,
                is_error: false,
            });
        }

        let result_limit_reached = relativized.len() > effective_limit;
        if result_limit_reached {
            relativized.truncate(effective_limit);
        }
        let raw_output = relativized.join("\n");
        let mut truncation = truncate_head(raw_output, usize::MAX, DEFAULT_MAX_BYTES);

        let mut result_output = std::mem::take(&mut truncation.content);
        let mut notices: Vec<String> = Vec::new();
        let mut details_map = serde_json::Map::new();

        if !status.success() {
            let code = status.code().unwrap_or(1);
            notices.push(format!("fd exited with code {code}"));
        }

        if result_limit_reached {
            notices.push(format!(
                "{effective_limit} results limit reached. Use limit={} for more, or refine pattern",
                effective_limit * 2
            ));
            details_map.insert(
                "resultLimitReached".to_string(),
                serde_json::Value::Number(serde_json::Number::from(effective_limit)),
            );
        }

        if truncation.truncated {
            notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
            details_map.insert("truncation".to_string(), serde_json::to_value(truncation)?);
        }

        if !notices.is_empty() {
            let _ = write!(result_output, "\n\n[{}]", notices.join(". "));
        }

        let details = if details_map.is_empty() {
            None
        } else {
            Some(serde_json::Value::Object(details_map))
        };

        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(result_output))],
            details,
            is_error: false,
        })
    }
}

// ============================================================================
// Ls Tool
// ============================================================================

/// Input parameters for the ls tool.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LsInput {
    path: Option<String>,
    limit: Option<usize>,
}

pub struct LsTool {
    cwd: PathBuf,
}

impl LsTool {
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
        }
    }
}

#[async_trait]
#[allow(clippy::unnecessary_literal_bound, clippy::too_many_lines)]
impl Tool for LsTool {
    fn name(&self) -> &str {
        "ls"
    }
    fn label(&self) -> &str {
        "ls"
    }
    fn description(&self) -> &str {
        "List directory contents. Returns entries sorted alphabetically, with '/' suffix for directories. Includes dotfiles. Output is truncated to 500 entries or 1MB (whichever is hit first)."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Directory to list (default: current directory)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of entries to return (default: 500)"
                }
            }
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let input: LsInput =
            serde_json::from_value(input).map_err(|e| Error::validation(e.to_string()))?;

        if matches!(input.limit, Some(0)) {
            return Err(Error::validation(
                "`limit` must be greater than 0".to_string(),
            ));
        }

        let dir_path = input
            .path
            .as_ref()
            .map_or_else(|| self.cwd.clone(), |p| resolve_read_path(p, &self.cwd));
        let dir_path = enforce_cwd_scope(&dir_path, &self.cwd, "list")?;

        let effective_limit = input.limit.unwrap_or(DEFAULT_LS_LIMIT);

        if !dir_path.exists() {
            return Err(Error::tool(
                "ls",
                format!("Path not found: {}", dir_path.display()),
            ));
        }
        if !dir_path.is_dir() {
            return Err(Error::tool(
                "ls",
                format!("Not a directory: {}", dir_path.display()),
            ));
        }

        let mut entries = Vec::new();
        let mut read_dir = asupersync::fs::read_dir(&dir_path)
            .await
            .map_err(|e| Error::tool("ls", format!("Cannot read directory: {e}")))?;

        let mut scan_limit_reached = false;
        while let Some(entry) = read_dir
            .next_entry()
            .await
            .map_err(|e| Error::tool("ls", format!("Cannot read directory entry: {e}")))?
        {
            if entries.len() >= LS_SCAN_HARD_LIMIT {
                scan_limit_reached = true;
                break;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            // Handle broken symlinks or permission errors by treating them as non-directories
            // Optimization: use file_type() first to avoid stat overhead on every file.
            let is_dir = match entry.file_type().await {
                Ok(ft) => {
                    if ft.is_dir() {
                        true
                    } else if ft.is_symlink() {
                        // Only stat if it's a symlink to see if it points to a directory
                        entry.metadata().await.is_ok_and(|meta| meta.is_dir())
                    } else {
                        false
                    }
                }
                Err(_) => entry.metadata().await.is_ok_and(|meta| meta.is_dir()),
            };
            entries.push((name, is_dir));
        }

        // Sort alphabetically (case-insensitive).
        entries.sort_by_cached_key(|(a, _)| a.to_lowercase());

        let mut results: Vec<String> = Vec::new();
        let mut entry_limit_reached = false;

        for (entry, is_dir) in entries {
            if results.len() >= effective_limit {
                entry_limit_reached = true;
                break;
            }
            if is_dir {
                results.push(format!("{entry}/"));
            } else {
                results.push(entry);
            }
        }

        if results.is_empty() {
            return Ok(ToolOutput {
                content: vec![ContentBlock::Text(TextContent::new("(empty directory)"))],
                details: None,
                is_error: false,
            });
        }

        // Apply byte truncation (no line limit since we already have entry limit).
        let raw_output = results.join("\n");
        let mut truncation = truncate_head(raw_output, usize::MAX, DEFAULT_MAX_BYTES);

        let mut output = std::mem::take(&mut truncation.content);
        let mut details_map = serde_json::Map::new();
        let mut notices: Vec<String> = Vec::new();

        if entry_limit_reached {
            notices.push(format!(
                "{effective_limit} entries limit reached. Use limit={} for more",
                effective_limit * 2
            ));
            details_map.insert(
                "entryLimitReached".to_string(),
                serde_json::Value::Number(serde_json::Number::from(effective_limit)),
            );
        }

        if scan_limit_reached {
            notices.push(format!(
                "Directory scan limited to {LS_SCAN_HARD_LIMIT} entries to prevent system overload"
            ));
            details_map.insert(
                "scanLimitReached".to_string(),
                serde_json::Value::Number(serde_json::Number::from(LS_SCAN_HARD_LIMIT)),
            );
        }

        if truncation.truncated {
            notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
            details_map.insert("truncation".to_string(), serde_json::to_value(truncation)?);
        }

        if !notices.is_empty() {
            let _ = write!(output, "\n\n[{}]", notices.join(". "));
        }

        let details = if details_map.is_empty() {
            None
        } else {
            Some(serde_json::Value::Object(details_map))
        };

        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(output))],
            details,
            is_error: false,
        })
    }
}

// ============================================================================
// Cleanup
// ============================================================================

/// Clean up old temporary files created by the bash tool.
///
/// Scans the system temporary directory for files matching `pi-bash-*.log`
/// that are older than 24 hours and deletes them. This prevents indefinite
/// accumulation of log files from long-running sessions.
pub fn cleanup_temp_files() {
    // Run in a detached thread to avoid blocking startup/shutdown.
    std::thread::spawn(|| {
        let temp_dir = std::env::temp_dir();
        let Ok(entries) = std::fs::read_dir(&temp_dir) else {
            return;
        };

        let now = std::time::SystemTime::now();
        let threshold = now
            .checked_sub(Duration::from_secs(24 * 60 * 60))
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }

            let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };

            // Match "pi-bash-" or "pi-rpc-bash-" prefix and ".log" suffix.
            if (file_name.starts_with("pi-bash-") || file_name.starts_with("pi-rpc-bash-"))
                && std::path::Path::new(file_name)
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("log"))
            {
                if let Ok(metadata) = entry.metadata() {
                    if let Ok(modified) = metadata.modified() {
                        if modified < threshold {
                            if let Err(e) = std::fs::remove_file(&path) {
                                // Log but don't panic on cleanup failure
                                tracing::debug!(
                                    "Failed to remove temp file {}: {}",
                                    path.display(),
                                    e
                                );
                            }
                        }
                    }
                }
            }
        }
    });
}

// ============================================================================
// Helper functions
// ============================================================================

fn rg_available() -> bool {
    find_rg_binary().is_some()
}

fn pump_stream<R: Read + Send + 'static>(
    mut reader: R,
    stream_name: &'static str,
    tx: &mpsc::SyncSender<BashPipeFrame>,
) {
    let mut buf = vec![0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if tx.send(BashPipeFrame::Chunk(buf[..n].to_vec())).is_err() {
                    break;
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(err) => {
                let _ = tx.send(BashPipeFrame::Error(format!(
                    "Failed to read bash {stream_name}: {err}"
                )));
                break;
            }
        }
    }
}

async fn ingest_bash_pipe_frame(frame: BashPipeFrame, state: &mut BashOutputState) -> Result<()> {
    match frame {
        BashPipeFrame::Chunk(chunk) => ingest_bash_chunk(chunk, state).await,
        BashPipeFrame::Error(message) => {
            let error_message = bash_capture_error_message(&message, state);
            state.abandon_spill_file();
            Err(Error::tool("bash", error_message))
        }
    }
}

fn bash_capture_error_message(message: &str, state: &BashOutputState) -> String {
    let raw = concat_chunks(&state.chunks);
    if raw.is_empty() {
        return message.to_string();
    }

    let full_text = String::from_utf8_lossy(&raw).into_owned();
    let truncation = truncate_tail(full_text, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
    let mut error_message = message.to_string();
    let partial_output = if truncation.content.is_empty() {
        "(no output)".to_string()
    } else {
        truncation.content
    };
    let _ = write!(
        error_message,
        "\n\nPartial output before failure:\n{partial_output}"
    );
    if truncation.truncated || state.total_bytes > state.chunks_bytes {
        let _ = write!(
            error_message,
            "\n\n[Partial output truncated before failure]"
        );
    }
    error_message
}

/// Read from a subprocess pipe until EOF while retaining only the first
/// `max_bytes + 1` bytes in memory so callers can detect truncation without
/// changing child-process behavior by closing the pipe early.
pub(crate) fn read_to_end_capped_and_drain<R: Read>(
    mut reader: R,
    max_bytes: u64,
) -> std::result::Result<Vec<u8>, String> {
    let capture_limit = usize::try_from(max_bytes.saturating_add(1)).unwrap_or(usize::MAX);
    let mut captured = Vec::with_capacity(capture_limit.min(8192));
    let mut chunk = [0u8; 8192];

    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => {
                let remaining = capture_limit.saturating_sub(captured.len());
                if remaining > 0 {
                    let keep = remaining.min(read);
                    captured.extend_from_slice(&chunk[..keep]);
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err.to_string()),
        }
    }

    Ok(captured)
}

// Keep `rx` as `&mut Receiver`: `std::sync::mpsc::Receiver` is `Send` but not
// `Sync`, and this helper awaits between polls, so `&Receiver` would make the
// surrounding future non-Send.
#[allow(clippy::needless_pass_by_ref_mut)]
async fn drain_bash_output(
    rx: &mut mpsc::Receiver<BashPipeFrame>,
    bash_output: &mut BashOutputState,
    cx: &AgentCx,
    drain_deadline: asupersync::Time,
    tick: Duration,
    allow_cancellation: bool,
) -> Result<bool> {
    loop {
        match rx.try_recv() {
            Ok(frame) => ingest_bash_pipe_frame(frame, bash_output).await?,
            Err(mpsc::TryRecvError::Empty) => {
                let now = cx
                    .cx()
                    .timer_driver()
                    .map_or_else(wall_now, |timer| timer.now());
                if now >= drain_deadline {
                    return Ok(false);
                }
                if allow_cancellation && cx.checkpoint().is_err() {
                    return Ok(true);
                }
                sleep(now, tick).await;
            }
            Err(mpsc::TryRecvError::Disconnected) => return Ok(false),
        }
    }
}

fn concat_chunks(chunks: &VecDeque<Vec<u8>>) -> Vec<u8> {
    let total: usize = chunks.iter().map(Vec::len).sum();
    let mut out = Vec::with_capacity(total);
    for chunk in chunks {
        out.extend_from_slice(chunk);
    }
    out
}

struct BashOutputState {
    total_bytes: usize,
    line_count: usize,
    last_byte_was_newline: bool,
    start_time: std::time::Instant,
    timeout_ms: Option<u64>,
    temp_file_path: Option<PathBuf>,
    temp_file: Option<asupersync::fs::File>,
    chunks: VecDeque<Vec<u8>>,
    chunks_bytes: usize,
    max_chunks_bytes: usize,
    spill_failed: bool,
}

impl BashOutputState {
    fn new(max_chunks_bytes: usize) -> Self {
        Self {
            total_bytes: 0,
            line_count: 0,
            last_byte_was_newline: false,
            start_time: std::time::Instant::now(),
            timeout_ms: None,
            temp_file_path: None,
            temp_file: None,
            chunks: VecDeque::new(),
            chunks_bytes: 0,
            max_chunks_bytes,
            spill_failed: false,
        }
    }

    fn abandon_spill_file(&mut self) {
        self.spill_failed = true;
        self.temp_file = None;
        if let Some(path) = self.temp_file_path.take() {
            if let Err(e) = std::fs::remove_file(&path)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                tracing::debug!(
                    "Failed to remove incomplete bash spill file {}: {}",
                    path.display(),
                    e
                );
            }
        }
    }
}

#[allow(clippy::too_many_lines)]
async fn ingest_bash_chunk(chunk: Vec<u8>, state: &mut BashOutputState) -> Result<()> {
    if chunk.is_empty() {
        return Ok(());
    }

    state.last_byte_was_newline = chunk.last().is_some_and(|byte| *byte == b'\n');
    state.total_bytes = state.total_bytes.saturating_add(chunk.len());
    state.line_count = state
        .line_count
        .saturating_add(memchr::memchr_iter(b'\n', &chunk).count());

    if state.total_bytes > DEFAULT_MAX_BYTES
        && state.temp_file.is_none()
        && state.temp_file_path.is_none()
        && !state.spill_failed
    {
        let id_full = Uuid::new_v4().simple().to_string();
        let id = &id_full[..16];
        let path = std::env::temp_dir().join(format!("pi-bash-{id}.log"));

        // Create the file synchronously with restricted permissions to avoid
        // a race condition where the file is world-readable before we fix it.
        // We also capture the inode (on Unix) to verify identity later.
        let path_clone = path.clone();
        let expected_inode: Option<u64> =
            asupersync::runtime::spawn_blocking_io(move || -> std::io::Result<Option<u64>> {
                let mut options = std::fs::OpenOptions::new();
                options.write(true).create_new(true);

                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o600);
                }

                match options.open(&path_clone) {
                    Ok(file) => {
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::MetadataExt;
                            Ok(file.metadata().ok().map(|m| m.ino()))
                        }
                        #[cfg(not(unix))]
                        {
                            drop(file);
                            Ok(None)
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Failed to create bash temp file: {e}");
                        Ok(None)
                    }
                }
            })
            .await
            .unwrap_or(None);

        if expected_inode.is_some() || !cfg!(unix) {
            match asupersync::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .await
            {
                Ok(mut file) => {
                    #[cfg_attr(not(unix), allow(unused_mut))]
                    let mut identity_match = true;
                    #[cfg(unix)]
                    if let Some(expected) = expected_inode {
                        use std::os::unix::fs::MetadataExt;
                        match file.metadata().await {
                            Ok(meta) => {
                                if meta.ino() != expected {
                                    tracing::warn!(
                                        "Temp file identity mismatch (possible TOCTOU attack)"
                                    );
                                    identity_match = false;
                                }
                            }
                            Err(e) => {
                                tracing::warn!("Failed to stat temp file: {e}");
                                identity_match = false;
                            }
                        }
                    }

                    if identity_match {
                        // Write buffered chunks to file first so it contains output from the beginning.
                        let mut failed_flush = false;
                        for existing in &state.chunks {
                            if let Err(e) = file.write_all(existing).await {
                                tracing::warn!("Failed to flush bash chunk to temp file: {e}");
                                failed_flush = true;
                                break;
                            }
                        }

                        state.temp_file_path = Some(path);
                        if failed_flush {
                            state.abandon_spill_file();
                        } else {
                            state.temp_file = Some(file);
                        }
                    } else {
                        state.temp_file_path = Some(path);
                        state.abandon_spill_file();
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to open temp file async: {e}");
                    state.temp_file_path = Some(path);
                    state.abandon_spill_file();
                }
            }
        } else {
            state.spill_failed = true;
        }
    }

    let mut close_spill_file = false;
    if let Some(file) = state.temp_file.as_mut() {
        let mut abandon_spill_file = false;
        if state.total_bytes <= BASH_FILE_LIMIT_BYTES {
            if let Err(e) = file.write_all(&chunk).await {
                tracing::warn!("Failed to write bash chunk to temp file: {e}");
                abandon_spill_file = true;
            }
        } else {
            // Hard limit reached. Stop writing and close the file to release the FD.
            if !state.spill_failed {
                tracing::warn!("Bash output exceeded hard limit; stopping file log");
                close_spill_file = true;
            }
        }
        if abandon_spill_file {
            state.abandon_spill_file();
        }
    }
    if close_spill_file {
        state.temp_file = None;
    }

    state.chunks_bytes = state.chunks_bytes.saturating_add(chunk.len());
    state.chunks.push_back(chunk);
    while state.chunks_bytes > state.max_chunks_bytes && state.chunks.len() > 1 {
        if let Some(front) = state.chunks.pop_front() {
            state.chunks_bytes = state.chunks_bytes.saturating_sub(front.len());
        }
    }
    Ok(())
}

const fn line_count_from_newline_count(
    total_bytes: usize,
    newline_count: usize,
    last_byte_was_newline: bool,
) -> usize {
    if total_bytes == 0 {
        0
    } else if last_byte_was_newline {
        newline_count
    } else {
        newline_count.saturating_add(1)
    }
}

fn emit_bash_update(
    state: &BashOutputState,
    on_update: Option<&(dyn Fn(ToolUpdate) + Send + Sync)>,
) -> Result<()> {
    if let Some(callback) = on_update {
        let raw = concat_chunks(&state.chunks);
        let full_text = String::from_utf8_lossy(&raw);
        let truncation =
            truncate_tail(full_text.into_owned(), DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);

        // Build the progress + details JSON using the json! macro instead of
        // manual Map::insert calls.  This eliminates 7+ String heap
        // allocations per update for the constant field-name keys
        // ("elapsedMs", "lineCount", …) that the manual path required.
        let elapsed_ms = state.start_time.elapsed().as_millis();
        let line_count = line_count_from_newline_count(
            state.total_bytes,
            state.line_count,
            state.last_byte_was_newline,
        );
        let mut details = serde_json::json!({
            "progress": {
                "elapsedMs": elapsed_ms,
                "lineCount": line_count,
                "byteCount": state.total_bytes
            }
        });
        let Some(details_map) = details.as_object_mut() else {
            return Ok(());
        };

        if let Some(timeout) = state.timeout_ms {
            if let Some(progress) = details_map
                .get_mut("progress")
                .and_then(|v| v.as_object_mut())
            {
                progress.insert("timeoutMs".into(), serde_json::json!(timeout));
            }
        }
        if truncation.truncated {
            details_map.insert("truncation".into(), serde_json::to_value(&truncation)?);
        }
        if let Some(path) = state.temp_file_path.as_ref() {
            details_map.insert(
                "fullOutputPath".into(),
                serde_json::Value::String(path.display().to_string()),
            );
        }

        callback(ToolUpdate {
            content: vec![ContentBlock::Text(TextContent::new(truncation.content))],
            details: Some(details),
        });
    }
    Ok(())
}

pub(crate) struct ProcessGuard {
    child: Option<std::process::Child>,
    cleanup_mode: ProcessCleanupMode,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProcessCleanupMode {
    ChildOnly,
    ProcessGroupTree,
}

impl ProcessGuard {
    pub(crate) const fn new(child: std::process::Child, cleanup_mode: ProcessCleanupMode) -> Self {
        Self {
            child: Some(child),
            cleanup_mode,
        }
    }

    pub(crate) fn try_wait_child(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.child
            .as_mut()
            .map_or(Ok(None), std::process::Child::try_wait)
    }

    pub(crate) fn kill(&mut self) -> Option<std::process::ExitStatus> {
        if let Some(mut child) = self.child.take() {
            cleanup_child(Some(child.id()), self.cleanup_mode);
            let _ = child.kill();
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            // We cannot return the exit status synchronously without blocking,
            // so we return None to indicate the process was forcefully killed.
            return None;
        }
        None
    }

    pub(crate) fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        if let Some(mut child) = self.child.take() {
            return child.wait();
        }
        Err(std::io::Error::other("Already waited"))
    }
}

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            match child.try_wait() {
                Ok(None) => {}
                Ok(Some(_)) | Err(_) => return,
            }
            let cleanup_mode = self.cleanup_mode;
            std::thread::spawn(move || {
                cleanup_child(Some(child.id()), cleanup_mode);
                let _ = child.kill();
                let _ = child.wait();
            });
        }
    }
}

fn cleanup_child(pid: Option<u32>, cleanup_mode: ProcessCleanupMode) {
    if cleanup_mode == ProcessCleanupMode::ProcessGroupTree {
        kill_process_group_tree(pid);
    }
}

pub fn kill_process_tree(pid: Option<u32>) {
    kill_process_tree_with(pid, sysinfo::Signal::Kill, false);
}

pub(crate) fn kill_process_group_tree(pid: Option<u32>) {
    kill_process_tree_with(pid, sysinfo::Signal::Kill, true);
}

fn terminate_process_group_tree(pid: Option<u32>) {
    kill_process_tree_with(pid, sysinfo::Signal::Term, true);
}

fn kill_process_tree_with(pid: Option<u32>, signal: sysinfo::Signal, include_process_group: bool) {
    let Some(pid) = pid else {
        return;
    };

    let root = sysinfo::Pid::from_u32(pid);

    let mut sys = sysinfo::System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);

    let mut children_map: HashMap<sysinfo::Pid, Vec<sysinfo::Pid>> = HashMap::new();
    for (p, proc_) in sys.processes() {
        if let Some(parent) = proc_.parent() {
            children_map.entry(parent).or_default().push(*p);
        }
    }

    let mut to_kill = Vec::new();
    let mut visited = std::collections::HashSet::new();
    collect_process_tree(root, &children_map, &mut to_kill, &mut visited);

    if include_process_group {
        // Some subprocess surfaces isolate the child into its own process group.
        // When they do, killing the group first catches background children even
        // if they have already been reparented away from the original root PID.
        #[cfg(unix)]
        {
            let sig_num = match signal {
                sysinfo::Signal::Kill => "9",
                _ => "15",
            };
            let _ = Command::new("kill")
                .arg(format!("-{sig_num}"))
                .arg("--")
                .arg(format!("-{pid}"))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }

    // Kill children first.
    for pid in to_kill.into_iter().rev() {
        if let Some(proc_) = sys.process(pid) {
            match proc_.kill_with(signal) {
                Some(true) => {}
                Some(false) | None => {
                    let _ = proc_.kill();
                }
            }
        }
    }
}

fn collect_process_tree(
    pid: sysinfo::Pid,
    children_map: &HashMap<sysinfo::Pid, Vec<sysinfo::Pid>>,
    out: &mut Vec<sysinfo::Pid>,
    visited: &mut std::collections::HashSet<sysinfo::Pid>,
) {
    if !visited.insert(pid) {
        return;
    }
    out.push(pid);
    if let Some(children) = children_map.get(&pid) {
        for child in children {
            collect_process_tree(*child, children_map, out, visited);
        }
    }
}

/// Build a child command whose Unix process image starts with SIGPIPE restored
/// to the platform default, without using `Command::pre_exec`.
///
/// Rust binaries ignore SIGPIPE by default, and POSIX inherits that disposition
/// across `exec(2)`. The tiny `/bin/sh` trampoline resets PIPE and then `exec`s
/// the requested program, preserving argv, cwd, stdio, and the process id that
/// later becomes the isolated process-group leader.
pub(crate) const SIGPIPE_TRAMPOLINE_EXEC_FAILURE_PREFIX: &str = "pi-sigpipe-reset: exec failed:";

pub(crate) fn command_with_default_sigpipe(program: impl AsRef<OsStr>) -> std::io::Result<Command> {
    command_with_default_sigpipe_for_cwd(program.as_ref(), None)
}

/// Variant of [`command_with_default_sigpipe`] for commands that will run with
/// `current_dir(cwd)`. This preserves relative `./program` lookup semantics.
pub(crate) fn command_with_default_sigpipe_in_dir(
    program: impl AsRef<OsStr>,
    cwd: &Path,
) -> std::io::Result<Command> {
    command_with_default_sigpipe_for_cwd(program.as_ref(), Some(cwd))
}

#[cfg(unix)]
fn command_with_default_sigpipe_for_cwd(
    program: &OsStr,
    cwd: Option<&Path>,
) -> std::io::Result<Command> {
    let program = resolve_executable_for_shell_trampoline(program, cwd)?;
    let mut command = Command::new("/bin/sh");
    command
        .arg("-c")
        .arg(
            "trap - PIPE\n\
             exec \"$@\"\n\
             status=$?\n\
             printf 'pi-sigpipe-reset: exec failed: %s\\n' \"$1\" >&2\n\
             exit \"$status\"",
        )
        .arg("pi-sigpipe-reset")
        .arg(program);
    Ok(command)
}

#[cfg(not(unix))]
fn command_with_default_sigpipe_for_cwd(
    program: &OsStr,
    _cwd: Option<&Path>,
) -> std::io::Result<Command> {
    Ok(Command::new(program))
}

#[cfg(unix)]
fn resolve_executable_for_shell_trampoline(
    program: &OsStr,
    cwd: Option<&Path>,
) -> std::io::Result<OsString> {
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::PermissionsExt as _;

    fn executable_candidate(path: &Path) -> std::io::Result<bool> {
        let metadata = std::fs::metadata(path)?;
        Ok(metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
    }

    fn absolutize_candidate(path: &Path, cwd: Option<&Path>) -> std::io::Result<PathBuf> {
        if path.is_absolute() {
            return Ok(path.to_path_buf());
        }

        let base = std::env::current_dir()?;
        Ok(cwd.map_or_else(|| base.join(path), |cwd| base.join(cwd).join(path)))
    }

    if program.as_bytes().contains(&b'/') {
        let path = Path::new(program);
        let candidate = absolutize_candidate(path, cwd)?;
        if executable_candidate(&candidate)? {
            return Ok(candidate.into_os_string());
        }
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("not an executable file: {}", candidate.display()),
        ));
    }

    let mut permission_denied = false;
    let paths = std::env::var_os("PATH").unwrap_or_else(|| OsString::from("/bin:/usr/bin"));
    for dir in std::env::split_paths(&paths) {
        let candidate = absolutize_candidate(&dir.join(program), cwd)?;
        match executable_candidate(&candidate) {
            Ok(true) => return Ok(candidate.into_os_string()),
            Ok(false) => permission_denied = true,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
                permission_denied = true;
            }
            Err(_) => {}
        }
    }

    if permission_denied {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("command is not executable: {}", program.to_string_lossy()),
        ))
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("command not found: {}", program.to_string_lossy()),
        ))
    }
}

/// Detach a child process from pi's controlling terminal.
pub(crate) fn isolate_command_process_group(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }

    #[cfg(not(unix))]
    {
        let _ = command;
    }
}

fn format_grep_path(file_path: &Path, cwd: &Path) -> String {
    if let Ok(rel) = file_path.strip_prefix(cwd) {
        let rel_str = rel.display().to_string().replace('\\', "/");
        if !rel_str.is_empty() {
            return rel_str;
        }
    }

    let canonical_file = safe_canonicalize(file_path);
    let canonical_cwd = safe_canonicalize(cwd);
    if let Ok(rel) = canonical_file.strip_prefix(&canonical_cwd) {
        let rel_str = rel.display().to_string().replace('\\', "/");
        if !rel_str.is_empty() {
            return rel_str;
        }
    }

    file_path.display().to_string().replace('\\', "/")
}

async fn get_file_lines_async<'a>(
    path: &Path,
    cache: &'a mut HashMap<PathBuf, Vec<String>>,
) -> &'a [String] {
    if !cache.contains_key(path) {
        // Prevent OOM on huge files and hangs on pipes
        if let Ok(meta) = asupersync::fs::metadata(path).await {
            if !meta.is_file() || meta.len() > 10 * 1024 * 1024 {
                cache.insert(path.to_path_buf(), Vec::new());
                return &[];
            }
        } else {
            cache.insert(path.to_path_buf(), Vec::new());
            return &[];
        }

        // Match Node's `readFileSync(..., "utf-8")` behavior: decode lossily rather than failing.
        let bytes = match asupersync::fs::read(path).await {
            Ok(bytes) => bytes,
            Err(err) => {
                tracing::debug!("Failed to read grep file {}: {err}", path.display());
                cache.insert(path.to_path_buf(), Vec::new());
                return &[];
            }
        };
        let content = String::from_utf8_lossy(&bytes);
        let mut lines = Vec::new();
        for line in content.split('\n') {
            let trimmed = line.strip_suffix('\r').unwrap_or(line);
            for piece in trimmed.split('\r') {
                lines.push(piece.to_string());
            }
        }
        if content.ends_with('\n') && lines.last().is_some_and(std::string::String::is_empty) {
            lines.pop();
        }
        cache.insert(path.to_path_buf(), lines);
    }
    cache.get(path).unwrap().as_slice()
}

fn find_fd_binary() -> Option<&'static str> {
    static BINARY: OnceLock<Option<&'static str>> = OnceLock::new();
    *BINARY.get_or_init(|| {
        if std::process::Command::new("fd")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok()
        {
            return Some("fd");
        }
        if std::process::Command::new("fdfind")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok()
        {
            return Some("fdfind");
        }
        None
    })
}

fn find_rg_binary() -> Option<&'static str> {
    static BINARY: OnceLock<Option<&'static str>> = OnceLock::new();
    *BINARY.get_or_init(|| {
        if std::process::Command::new("rg")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok()
        {
            return Some("rg");
        }
        if std::process::Command::new("ripgrep")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok()
        {
            return Some("ripgrep");
        }
        None
    })
}

// ============================================================================
// Hashline Edit Tool
// ============================================================================

/// Custom nibble-encoding alphabet used for hashline tags.
const NIBBLE_STR: &[u8; 16] = b"ZPMQVRWSNKTXJBYH";

/// Pre-computed 256-entry lookup table mapping each byte value to its
/// 2-character NIBBLE_STR encoding.
static HASHLINE_DICT: OnceLock<[[u8; 2]; 256]> = OnceLock::new();

fn hashline_dict() -> &'static [[u8; 2]; 256] {
    HASHLINE_DICT.get_or_init(|| {
        let mut dict = [[0u8; 2]; 256];
        for i in 0..256 {
            dict[i] = [NIBBLE_STR[i & 0x0F], NIBBLE_STR[(i >> 4) & 0x0F]];
        }
        dict
    })
}

/// Compute a 2-character hash tag for a line at the given 0-indexed position.
///
/// The algorithm:
/// 1. Strip trailing `\r`
/// 2. Remove all whitespace to get a "significant" string
/// 3. If the significant string contains at least one letter or digit, seed = 0;
///    otherwise seed = line index (to disambiguate punctuation-only or blank lines)
/// 4. Compute `xxh32(significant_bytes, seed) & 0xFF`
/// 5. Encode the low byte as 2 nibble chars from `NIBBLE_STR`
fn compute_line_hash(line_idx: usize, line: &str) -> [u8; 2] {
    let line = line.strip_suffix('\r').unwrap_or(line);
    // Remove all whitespace
    let significant: String = line.chars().filter(|c| !c.is_whitespace()).collect();
    let has_alnum = significant.chars().any(char::is_alphanumeric);
    let seed = if has_alnum {
        0
    } else {
        #[allow(clippy::cast_possible_truncation)]
        let s = line_idx as u32;
        s
    };
    let hash = xxhash_rust::xxh32::xxh32(significant.as_bytes(), seed);
    let byte = (hash & 0xFF) as usize;
    hashline_dict()[byte]
}

/// Format a hashline tag as `"N#AB"` where N is the 1-indexed line number.
fn format_hashline_tag(line_idx: usize, line: &str) -> String {
    let h = compute_line_hash(line_idx, line);
    format!("{}#{}{}", line_idx + 1, h[0] as char, h[1] as char)
}

/// Compute a hashline tag, reapplying a stripped BOM for the first line if needed.
fn format_hashline_tag_with_bom(line_idx: usize, line: &str, had_bom: bool) -> String {
    let h = compute_line_hash_with_bom(line_idx, line, had_bom);
    format!("{}#{}{}", line_idx + 1, h[0] as char, h[1] as char)
}

fn compute_line_hash_with_bom(line_idx: usize, line: &str, had_bom: bool) -> [u8; 2] {
    if had_bom && line_idx == 0 {
        let mut with_bom = String::with_capacity(line.len().saturating_add(1));
        with_bom.push('\u{FEFF}');
        with_bom.push_str(line);
        compute_line_hash(line_idx, &with_bom)
    } else {
        compute_line_hash(line_idx, line)
    }
}

/// Regex for parsing hashline references like `5#KJ` or ` > +  5 # KJ `.
/// Tolerates leading whitespace, diff markers (`>`, `+`, `-`), and spaces around `#`.
static HASHLINE_TAG_RE: OnceLock<regex::Regex> = OnceLock::new();

fn hashline_tag_regex() -> &'static regex::Regex {
    HASHLINE_TAG_RE.get_or_init(|| {
        regex::Regex::new(r"^[\s>+\-]*(\d+)\s*#\s*([ZPMQVRWSNKTXJBYH]{2})")
            .expect("valid hashline regex")
    })
}

/// Parse a hashline tag reference string into (1-indexed line number, 2-byte hash).
fn parse_hashline_tag(ref_str: &str) -> std::result::Result<(usize, [u8; 2]), String> {
    let re = hashline_tag_regex();
    let caps = re
        .captures(ref_str)
        .ok_or_else(|| format!("Invalid hashline reference: {ref_str:?}"))?;
    let line_num: usize = caps[1]
        .parse()
        .map_err(|e| format!("Invalid line number in {ref_str:?}: {e}"))?;
    if line_num == 0 {
        return Err(format!("Line number must be >= 1, got 0 in {ref_str:?}"));
    }
    let hash_bytes = caps[2].as_bytes();
    Ok((line_num, [hash_bytes[0], hash_bytes[1]]))
}

/// Strip hashline tag prefixes that models sometimes copy into replacement content.
/// Matches patterns like `5#KJ:content` and returns just `content`.
static HASHLINE_PREFIX_RE: OnceLock<regex::Regex> = OnceLock::new();

fn strip_hashline_prefix(line: &str) -> &str {
    let re = HASHLINE_PREFIX_RE.get_or_init(|| {
        regex::Regex::new(r"^[\s>+\-]*\d+\s*#\s*[ZPMQVRWSNKTXJBYH]{2}\s*:")
            .expect("valid hashline prefix regex")
    });
    re.find(line).map_or(line, |m| &line[m.end()..])
}

/// Input parameters for the hashline edit tool.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HashlineEditInput {
    path: String,
    edits: Vec<HashlineOp>,
}

/// A single hashline edit operation.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HashlineOp {
    /// Operation type: "replace", "prepend", or "append"
    op: String,
    /// Start anchor in "LINE#HASH" format (optional for BOF prepend / EOF append)
    pos: Option<String>,
    /// End anchor for range replace (inclusive)
    end: Option<String>,
    /// Replacement / insertion lines
    lines: Option<serde_json::Value>,
}

impl HashlineOp {
    /// Extract lines from the `lines` field, handling string, array, and null variants.
    fn get_lines(&self) -> Vec<String> {
        match &self.lines {
            None | Some(serde_json::Value::Null) => vec![],
            Some(serde_json::Value::String(s)) => {
                normalize_to_lf(s).split('\n').map(String::from).collect()
            }
            Some(serde_json::Value::Array(arr)) => arr
                .iter()
                .map(|v| match v {
                    serde_json::Value::String(s) => normalize_to_lf(s),
                    other => normalize_to_lf(&other.to_string()),
                })
                .collect(),
            Some(other) => vec![normalize_to_lf(&other.to_string())],
        }
    }
}

/// A resolved hashline edit operation ready for application.
struct ResolvedEdit<'a> {
    op: &'a str,
    /// 0-indexed start line (or 0 for BOF, `file_lines.len()` for EOF)
    start: usize,
    /// 0-indexed end line (inclusive, same as start for single-line ops)
    end: usize,
    lines: Vec<String>,
}

pub struct HashlineEditTool {
    cwd: PathBuf,
}

impl HashlineEditTool {
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
        }
    }
}

/// Validate a hashline tag reference against actual file lines.
/// Returns `Ok(0-indexed line)` or `Err(message)` with context.
fn validate_line_ref(
    ref_str: &str,
    file_lines: &[&str],
    had_bom: bool,
) -> std::result::Result<usize, String> {
    let (line_num, expected_hash) = parse_hashline_tag(ref_str)?;
    let line_idx = line_num - 1;
    if line_idx >= file_lines.len() {
        return Err(format!(
            "Line {line_num} out of range (file has {} lines)",
            file_lines.len()
        ));
    }
    let actual_hash = compute_line_hash_with_bom(line_idx, file_lines[line_idx], had_bom);
    if actual_hash != expected_hash {
        let tag = format_hashline_tag_with_bom(line_idx, file_lines[line_idx], had_bom);
        return Err(format!(
            "Hash mismatch at line {line_num}: expected {}#{}{}, actual is {tag}",
            line_num, expected_hash[0] as char, expected_hash[1] as char,
        ));
    }
    Ok(line_idx)
}

/// Build a context snippet around a mismatched line for error reporting.
fn mismatch_context(file_lines: &[&str], line_idx: usize, context: usize, had_bom: bool) -> String {
    let start = line_idx.saturating_sub(context);
    let end = (line_idx + context + 1).min(file_lines.len());
    let mut out = String::new();
    for (i, &file_line) in file_lines.iter().enumerate().take(end).skip(start) {
        let tag = format_hashline_tag_with_bom(i, file_line, had_bom);
        if i == line_idx {
            let _ = writeln!(out, ">>> {tag}:{file_line}");
        } else {
            let _ = writeln!(out, "    {tag}:{file_line}");
        }
    }
    out
}

/// Collect all hash mismatches from a set of edits, returning a combined error message.
fn collect_mismatches(
    edits: &[HashlineOp],
    file_lines: &[&str],
    had_bom: bool,
) -> std::result::Result<(), String> {
    let mut errors = Vec::new();
    for edit in edits {
        if let Some(ref pos) = edit.pos {
            if let Err(e) = validate_line_ref(pos, file_lines, had_bom) {
                // Find the line index for context
                if let Ok((line_num, _)) = parse_hashline_tag(pos) {
                    let idx = (line_num - 1).min(file_lines.len().saturating_sub(1));
                    errors.push(format!(
                        "{e}\n{}",
                        mismatch_context(file_lines, idx, 2, had_bom)
                    ));
                } else {
                    errors.push(e);
                }
            }
        }
        if let Some(ref end) = edit.end {
            if let Err(e) = validate_line_ref(end, file_lines, had_bom) {
                if let Ok((line_num, _)) = parse_hashline_tag(end) {
                    let idx = (line_num - 1).min(file_lines.len().saturating_sub(1));
                    errors.push(format!(
                        "{e}\n{}",
                        mismatch_context(file_lines, idx, 2, had_bom)
                    ));
                } else {
                    errors.push(e);
                }
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}

/// Normalized representation of an edit for deduplication.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct NormalizedEdit {
    op: String,
    pos_line: Option<usize>,
    end_line: Option<usize>,
    lines: Vec<String>,
}

/// Sort precedence for overlapping edits at the same line.
fn op_precedence(op: &str) -> u8 {
    match op {
        "replace" => 0,
        "append" => 1,
        "prepend" => 2,
        _ => 3,
    }
}

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Tool for HashlineEditTool {
    fn name(&self) -> &str {
        "hashline_edit"
    }
    fn label(&self) -> &str {
        "hashline edit"
    }
    fn description(&self) -> &str {
        "Apply precise file edits using LINE#HASH tags from a prior read with hashline=true. \
         Each edit specifies an op (replace/prepend/append), a pos anchor (\"N#AB\"), an optional \
         end anchor for range replace, and replacement lines. Edits are validated against current \
         file hashes and applied bottom-up to avoid index invalidation."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to edit (relative or absolute)"
                },
                "edits": {
                    "type": "array",
                    "description": "Array of edit operations to apply",
                    "items": {
                        "type": "object",
                        "properties": {
                            "op": {
                                "type": "string",
                                "enum": ["replace", "prepend", "append"],
                                "description": "Operation type"
                            },
                            "pos": {
                                "type": "string",
                                "description": "Anchor line reference in LINE#HASH format (e.g. \"5#KJ\")"
                            },
                            "end": {
                                "type": "string",
                                "description": "End anchor for range replace (inclusive)"
                            },
                            "lines": {
                                "description": "Replacement/insertion content as array of strings, single string, or null for deletion",
                                "oneOf": [
                                    { "type": "array", "items": { "type": "string" } },
                                    { "type": "string" },
                                    { "type": "null" }
                                ]
                            }
                        },
                        "required": ["op"]
                    }
                }
            },
            "required": ["path", "edits"]
        })
    }

    #[allow(clippy::too_many_lines)]
    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let input: HashlineEditInput = serde_json::from_value(input)
            .map_err(|e| Error::tool("hashline_edit", format!("Invalid input: {e}")))?;

        if input.edits.is_empty() {
            return Err(Error::tool("hashline_edit", "No edits provided"));
        }

        // Resolve file path and enforce scope before touching the filesystem.
        let resolved = resolve_read_path(&input.path, &self.cwd);
        let absolute_path = enforce_cwd_scope(&resolved, &self.cwd, "hashline_edit")?;

        // Check file size
        let metadata = asupersync::fs::metadata(&absolute_path)
            .await
            .map_err(|err| {
                let message = match err.kind() {
                    std::io::ErrorKind::NotFound => format!("File not found: {}", input.path),
                    std::io::ErrorKind::PermissionDenied => {
                        format!("Permission denied: {}", input.path)
                    }
                    _ => format!("Cannot read file metadata: {err}"),
                };
                Error::tool("hashline_edit", message)
            })?;
        if !metadata.is_file() {
            return Err(Error::tool(
                "hashline_edit",
                format!("Path {} is not a regular file", absolute_path.display()),
            ));
        }
        if metadata.len() > READ_TOOL_MAX_BYTES {
            return Err(Error::tool(
                "hashline_edit",
                format!(
                    "File too large ({} bytes, max {} bytes)",
                    metadata.len(),
                    READ_TOOL_MAX_BYTES
                ),
            ));
        }

        // Read file content
        let file = asupersync::fs::File::open(&absolute_path)
            .await
            .map_err(|e| Error::tool("hashline_edit", format!("Cannot open file: {e}")))?;
        let mut raw = Vec::new();
        let mut limiter = file.take(READ_TOOL_MAX_BYTES.saturating_add(1));
        limiter
            .read_to_end(&mut raw)
            .await
            .map_err(|e| Error::tool("hashline_edit", format!("Cannot read file: {e}")))?;

        if raw.len() as u64 > READ_TOOL_MAX_BYTES {
            return Err(Error::tool(
                "hashline_edit",
                format!("File too large (> {READ_TOOL_MAX_BYTES} bytes)"),
            ));
        }

        let raw_content = String::from_utf8(raw).map_err(|_| {
            Error::tool(
                "hashline_edit",
                "File contains invalid UTF-8 characters and cannot be safely edited as text."
                    .to_string(),
            )
        })?;

        let (content_no_bom, had_bom) = strip_bom(&raw_content);
        let original_ending = detect_line_ending(content_no_bom);
        let normalized = normalize_to_lf(content_no_bom);
        let file_lines: Vec<&str> = normalized.split('\n').collect();

        // Validate all hash references before making any changes
        if let Err(e) = collect_mismatches(&input.edits, &file_lines, had_bom) {
            return Err(Error::tool(
                "hashline_edit",
                format!("Hash validation failed — re-read the file to get current tags.\n\n{e}"),
            ));
        }

        // Deduplicate edits
        let mut seen = std::collections::HashSet::new();
        let mut deduped_edits: Vec<&HashlineOp> = Vec::new();
        for edit in &input.edits {
            let pos_line = edit
                .pos
                .as_ref()
                .and_then(|p| parse_hashline_tag(p).ok())
                .map(|(n, _)| n);
            let end_line = edit
                .end
                .as_ref()
                .and_then(|e| parse_hashline_tag(e).ok())
                .map(|(n, _)| n);
            let key = NormalizedEdit {
                op: edit.op.clone(),
                pos_line,
                end_line,
                lines: edit.get_lines(),
            };
            if seen.insert(key) {
                deduped_edits.push(edit);
            }
        }

        // Resolve line indices and sort bottom-up
        let mut resolved: Vec<ResolvedEdit<'_>> = Vec::new();
        for edit in &deduped_edits {
            let replacement_lines: Vec<String> = edit
                .get_lines()
                .into_iter()
                .map(|l| strip_hashline_prefix(&l).to_string())
                .collect();

            match edit.op.as_str() {
                "replace" => {
                    let start_idx = match &edit.pos {
                        Some(pos) => validate_line_ref(pos, &file_lines, had_bom)
                            .map_err(|e| Error::tool("hashline_edit", e))?,
                        None => {
                            return Err(Error::tool(
                                "hashline_edit",
                                "replace operation requires a pos anchor",
                            ));
                        }
                    };
                    let end_idx = match &edit.end {
                        Some(end) => validate_line_ref(end, &file_lines, had_bom)
                            .map_err(|e| Error::tool("hashline_edit", e))?,
                        None => start_idx,
                    };
                    if end_idx < start_idx {
                        return Err(Error::tool(
                            "hashline_edit",
                            format!(
                                "End anchor (line {}) is before start anchor (line {})",
                                end_idx + 1,
                                start_idx + 1
                            ),
                        ));
                    }
                    resolved.push(ResolvedEdit {
                        op: "replace",
                        start: start_idx,
                        end: end_idx,
                        lines: replacement_lines,
                    });
                }
                "prepend" => {
                    let idx = match &edit.pos {
                        Some(pos) => validate_line_ref(pos, &file_lines, had_bom)
                            .map_err(|e| Error::tool("hashline_edit", e))?,
                        None => 0, // BOF
                    };
                    let end_idx = if file_lines == [""] && edit.pos.is_none() {
                        0 // replace the empty line
                    } else {
                        idx
                    };
                    resolved.push(ResolvedEdit {
                        op: if file_lines == [""] && edit.pos.is_none() {
                            "replace"
                        } else {
                            "prepend"
                        },
                        start: idx,
                        end: end_idx,
                        lines: replacement_lines,
                    });
                }
                "append" => {
                    let idx = match &edit.pos {
                        Some(pos) => validate_line_ref(pos, &file_lines, had_bom)
                            .map_err(|e| Error::tool("hashline_edit", e))?,
                        None => {
                            if file_lines.len() > 1 && file_lines.last() == Some(&"") {
                                file_lines.len() - 2
                            } else {
                                file_lines.len().saturating_sub(1)
                            }
                        }
                    };
                    let end_idx = if file_lines == [""] && edit.pos.is_none() {
                        0 // replace the empty line
                    } else {
                        idx
                    };
                    resolved.push(ResolvedEdit {
                        op: if file_lines == [""] && edit.pos.is_none() {
                            "replace"
                        } else {
                            "append"
                        },
                        start: idx,
                        end: end_idx,
                        lines: replacement_lines,
                    });
                }
                other => {
                    return Err(Error::tool(
                        "hashline_edit",
                        format!("Unknown op: {other:?}. Must be replace, prepend, or append."),
                    ));
                }
            }
        }

        // Sort bottom-up: highest line first, then by precedence (replace < append < prepend)
        resolved.sort_by(|a, b| {
            b.start
                .cmp(&a.start)
                .then_with(|| op_precedence(a.op).cmp(&op_precedence(b.op)))
        });

        // Detect overlapping edit ranges (undefined behavior if applied bottom-up)
        for i in 0..resolved.len() {
            for j in (i + 1)..resolved.len() {
                let a = &resolved[i];
                let b = &resolved[j];
                if a.start <= b.end && b.start <= a.end {
                    return Err(Error::tool(
                        "hashline_edit",
                        format!(
                            "Overlapping edits detected: {} at line {}-{} and {} at line {}-{}. \
                             Please combine overlapping edits into a single operation.",
                            a.op,
                            a.start + 1,
                            a.end + 1,
                            b.op,
                            b.start + 1,
                            b.end + 1
                        ),
                    ));
                }
            }
        }

        // Apply splices bottom-up on a mutable Vec of lines
        let mut lines: Vec<String> = file_lines.iter().map(|s| (*s).to_string()).collect();
        let mut any_change = false;

        for edit in &resolved {
            match edit.op {
                "replace" => {
                    // Check if it's a no-op
                    let existing: Vec<&str> = lines[edit.start..=edit.end]
                        .iter()
                        .map(String::as_str)
                        .collect();
                    if existing == edit.lines.iter().map(String::as_str).collect::<Vec<&str>>() {
                        continue; // no-op
                    }
                    // Splice: remove old range, insert new lines
                    lines.splice(edit.start..=edit.end, edit.lines.iter().cloned());
                    any_change = true;
                }
                "prepend" => {
                    // Insert before the target line
                    lines.splice(edit.start..edit.start, edit.lines.iter().cloned());
                    if !edit.lines.is_empty() {
                        any_change = true;
                    }
                }
                "append" => {
                    // Insert after the target line
                    let insert_at = edit.start + 1;
                    lines.splice(insert_at..insert_at, edit.lines.iter().cloned());
                    if !edit.lines.is_empty() {
                        any_change = true;
                    }
                }
                _ => {} // unreachable due to earlier validation
            }
        }

        if !any_change {
            return Err(Error::tool(
                "hashline_edit",
                format!(
                    "No changes made to {}. All edits were no-ops (replacement identical to existing content).",
                    input.path
                ),
            ));
        }

        // Reconstruct content
        let new_normalized = lines.join("\n");
        let new_content = restore_line_endings(&new_normalized, original_ending);
        let mut final_content = new_content;
        if had_bom {
            final_content = format!("\u{FEFF}{final_content}");
        }

        // Atomic write (same pattern as EditTool)
        let absolute_path_clone = absolute_path.clone();
        let final_content_bytes = final_content.into_bytes();
        asupersync::runtime::spawn_blocking_io(move || {
            let original_perms = std::fs::metadata(&absolute_path_clone)
                .ok()
                .map(|m| m.permissions());
            let parent = absolute_path_clone
                .parent()
                .unwrap_or_else(|| Path::new("."));
            let mut temp_file = tempfile::NamedTempFile::new_in(parent)?;

            temp_file.as_file_mut().write_all(&final_content_bytes)?;
            temp_file.as_file_mut().sync_all()?;

            if let Some(perms) = original_perms {
                let _ = temp_file.as_file().set_permissions(perms);
            } else {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = temp_file
                        .as_file()
                        .set_permissions(std::fs::Permissions::from_mode(0o644));
                }
            }

            temp_file
                .persist(&absolute_path_clone)
                .map_err(|e| e.error)?;
            Ok(())
        })
        .await
        .map_err(|e| Error::tool("hashline_edit", format!("Failed to write file: {e}")))?;

        // Generate diff
        let (diff, first_changed_line) = generate_diff_string(&normalized, &new_normalized);
        let mut details = serde_json::Map::new();
        details.insert("diff".to_string(), serde_json::Value::String(diff));
        if let Some(line) = first_changed_line {
            details.insert(
                "firstChangedLine".to_string(),
                serde_json::Value::Number(serde_json::Number::from(line)),
            );
        }

        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(format!(
                "Successfully applied hashline edits to {}.",
                input.path
            )))],
            details: Some(serde_json::Value::Object(details)),
            is_error: false,
        })
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    #[cfg(target_os = "linux")]
    use std::time::Duration;

    #[test]
    fn test_truncate_head() {
        let content = "line1\nline2\nline3\nline4\nline5".to_string();
        let result = truncate_head(content, 3, 1000);

        assert_eq!(result.content, "line1\nline2\nline3\n");
        assert!(result.truncated);
        assert_eq!(result.truncated_by, Some(TruncatedBy::Lines));
        assert_eq!(result.total_lines, 5);
        assert_eq!(result.output_lines, 3);
    }

    #[test]
    fn test_truncate_tail() {
        let content = "line1\nline2\nline3\nline4\nline5".to_string();
        let result = truncate_tail(content, 3, 1000);

        assert_eq!(result.content, "line3\nline4\nline5");
        assert!(result.truncated);
        assert_eq!(result.truncated_by, Some(TruncatedBy::Lines));
        assert_eq!(result.total_lines, 5);
        assert_eq!(result.output_lines, 3);
    }

    #[test]
    fn test_truncate_tail_zero_lines_returns_empty_output() {
        let result = truncate_tail("line1\nline2".to_string(), 0, 1000);

        assert!(result.truncated);
        assert_eq!(result.truncated_by, Some(TruncatedBy::Lines));
        assert_eq!(result.output_lines, 0);
        assert_eq!(result.output_bytes, 0);
        assert!(result.content.is_empty());
    }

    #[test]
    fn test_line_count_from_newline_count_matches_trailing_newline_semantics() {
        assert_eq!(line_count_from_newline_count(0, 0, false), 0);
        assert_eq!(line_count_from_newline_count(2, 1, true), 1);
        assert_eq!(line_count_from_newline_count(1, 0, false), 1);
        assert_eq!(line_count_from_newline_count(3, 1, false), 2);
    }

    #[test]
    fn test_rg_match_requires_path_and_line_number() {
        let mut matches = Vec::new();
        let mut match_count = 0usize;
        let mut match_limit_reached = false;
        let scan_limit = 1;

        let missing_line =
            Ok(r#"{"type":"match","data":{"path":{"text":"file.txt"}}}"#.to_string());
        process_rg_json_match_line(
            missing_line,
            &mut matches,
            &mut match_count,
            &mut match_limit_reached,
            scan_limit,
        );
        assert!(matches.is_empty());
        assert_eq!(match_count, 0);
        assert!(!match_limit_reached);

        let valid_line = Ok(
            r#"{"type":"match","data":{"path":{"text":"file.txt"},"line_number":3}}"#.to_string(),
        );
        process_rg_json_match_line(
            valid_line,
            &mut matches,
            &mut match_count,
            &mut match_limit_reached,
            scan_limit,
        );
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].1, 3);
        assert_eq!(match_count, 1);
        assert!(match_limit_reached);
    }

    #[test]
    fn test_truncate_by_bytes() {
        let content = "short\nthis is a longer line\nanother".to_string();
        let result = truncate_head(content, 100, 15);

        assert!(result.truncated);
        assert_eq!(result.truncated_by, Some(TruncatedBy::Bytes));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_command_with_default_sigpipe_restores_pipe_disposition() {
        let output = command_with_default_sigpipe("sh")
            .expect("prepare sigpipe disposition probe")
            .args([
                "-c",
                "while read name value _; do [ \"$name\" = SigIgn: ] && { printf '%s' \"$value\"; exit 0; }; done < /proc/$$/status",
            ])
            .stdout(std::process::Stdio::piped())
            .output()
            .expect("spawn sigpipe disposition probe");

        assert!(output.status.success(), "probe failed: {output:?}");
        let sigign = String::from_utf8(output.stdout).expect("SigIgn should be utf8");
        let ignored_mask =
            u64::from_str_radix(sigign.trim(), 16).expect("SigIgn should be a hex mask");
        let sigpipe_bit = 1_u64 << (13 - 1);
        assert_eq!(
            ignored_mask & sigpipe_bit,
            0,
            "child should not inherit ignored SIGPIPE: SigIgn={sigign}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_command_with_default_sigpipe_in_dir_resolves_relative_program_after_cwd() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = tempfile::tempdir().expect("create temp dir");
        let script = tmp.path().join("relative-probe");
        std::fs::write(&script, "#!/bin/sh\nprintf cwd-relative-ok\n").expect("write script");
        let mut permissions = std::fs::metadata(&script)
            .expect("stat script")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).expect("make script executable");

        let output = command_with_default_sigpipe_in_dir("./relative-probe", tmp.path())
            .expect("prepare relative executable")
            .current_dir(tmp.path())
            .stdout(std::process::Stdio::piped())
            .output()
            .expect("spawn relative executable");

        assert!(output.status.success(), "probe failed: {output:?}");
        assert_eq!(
            String::from_utf8(output.stdout).expect("probe stdout should be utf8"),
            "cwd-relative-ok"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_read_to_end_capped_and_drain_preserves_writer_exit_status() {
        let mut child = std::process::Command::new("dd")
            .args(["if=/dev/zero", "bs=1", "count=70000", "status=none"])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("spawn dd");

        let stdout = child.stdout.take().expect("dd stdout");
        let captured = read_to_end_capped_and_drain(stdout, 1024).expect("capture bounded stdout");
        let status = child.wait().expect("wait for dd");

        assert!(
            status.success(),
            "bounded reader should drain to EOF instead of SIGPIPEing the writer: {status:?}"
        );
        assert_eq!(captured.len(), 1025);
    }

    #[cfg(unix)]
    #[test]
    fn test_get_file_lines_async_unreadable_file_returns_empty() {
        asupersync::test_utils::run_test(|| async {
            use std::os::unix::fs::PermissionsExt;

            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("secret.txt");
            std::fs::write(&path, "secret\n").unwrap();

            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o000);
            std::fs::set_permissions(&path, perms).unwrap();

            let mut cache = HashMap::new();
            let lines = get_file_lines_async(&path, &mut cache).await;
            assert!(lines.is_empty());
        });
    }

    #[test]
    fn test_resolve_path_absolute() {
        let cwd = PathBuf::from("/home/user/project");
        let result = resolve_path("/absolute/path", &cwd);
        assert_eq!(result, PathBuf::from("/absolute/path"));
    }

    #[test]
    fn test_resolve_path_relative() {
        let cwd = PathBuf::from("/home/user/project");
        let result = resolve_path("src/main.rs", &cwd);
        assert_eq!(result, PathBuf::from("/home/user/project/src/main.rs"));
    }

    #[test]
    fn test_normalize_dot_segments_preserves_root() {
        let result = normalize_dot_segments(std::path::Path::new("/../etc/passwd"));
        assert_eq!(result, PathBuf::from("/etc/passwd"));
    }

    #[test]
    fn test_normalize_dot_segments_preserves_leading_parent_for_relative() {
        let result = normalize_dot_segments(std::path::Path::new("../a/../b"));
        assert_eq!(result, PathBuf::from("../b"));
    }

    #[test]
    fn test_detect_supported_image_mime_type_from_bytes() {
        assert_eq!(
            detect_supported_image_mime_type_from_bytes(b"\x89PNG\r\n\x1A\n"),
            Some("image/png")
        );
        assert_eq!(
            detect_supported_image_mime_type_from_bytes(b"\xFF\xD8\xFF"),
            Some("image/jpeg")
        );
        assert_eq!(
            detect_supported_image_mime_type_from_bytes(b"GIF89a"),
            Some("image/gif")
        );
        assert_eq!(
            detect_supported_image_mime_type_from_bytes(b"RIFF1234WEBP"),
            Some("image/webp")
        );
        assert_eq!(
            detect_supported_image_mime_type_from_bytes(b"not an image"),
            None
        );
    }

    #[test]
    fn test_format_size() {
        assert_eq!(format_size(500), "500B");
        assert_eq!(format_size(1024), "1.0KB");
        assert_eq!(format_size(1536), "1.5KB");
        assert_eq!(format_size(1_048_576), "1.0MB");
        assert_eq!(format_size(1_073_741_824), "1024.0MB");
    }

    #[test]
    fn test_js_string_length() {
        assert_eq!(js_string_length("hello"), 5);
        assert_eq!(js_string_length("😀"), 2);
    }

    #[test]
    fn test_truncate_line() {
        let short = "short line";
        let result = truncate_line(short, 100);
        assert_eq!(result.text, "short line");
        assert!(!result.was_truncated);

        let long = "a".repeat(600);
        let result = truncate_line(&long, 500);
        assert!(result.was_truncated);
        assert!(result.text.ends_with("... [truncated]"));
    }

    // ========================================================================
    // Helper: extract text from ToolOutput content blocks
    // ========================================================================

    fn get_text(content: &[ContentBlock]) -> String {
        content
            .iter()
            .filter_map(|block| {
                if let ContentBlock::Text(text) = block {
                    Some(text.text.clone())
                } else {
                    None
                }
            })
            .collect::<String>()
    }

    // ========================================================================
    // Read Tool Tests
    // ========================================================================

    #[test]
    fn test_read_valid_file() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::write(tmp.path().join("hello.txt"), "alpha\nbeta\ngamma").unwrap();

            let tool = ReadTool::new(tmp.path());
            let out = tool
                .execute(
                    "t",
                    serde_json::json!({ "path": tmp.path().join("hello.txt").to_string_lossy() }),
                    None,
                )
                .await
                .unwrap();
            let text = get_text(&out.content);
            assert!(text.contains("alpha"));
            assert!(text.contains("beta"));
            assert!(text.contains("gamma"));
            assert!(!out.is_error);
        });
    }

    #[test]
    fn test_read_nonexistent_file() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            let tool = ReadTool::new(tmp.path());
            let err = tool
                .execute(
                    "t",
                    serde_json::json!({ "path": tmp.path().join("nope.txt").to_string_lossy() }),
                    None,
                )
                .await;
            assert!(err.is_err());
        });
    }

    #[test]
    fn test_read_rejects_outside_cwd() {
        asupersync::test_utils::run_test(|| async {
            let cwd = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            std::fs::write(outside.path().join("secret.txt"), "secret").unwrap();

            let tool = ReadTool::new(cwd.path());
            let err = tool
                .execute(
                    "t",
                    serde_json::json!({ "path": outside.path().join("secret.txt").to_string_lossy() }),
                    None,
                )
                .await
                .unwrap_err();
            assert!(err.to_string().contains("outside the working directory"));
        });
    }

    #[test]
    fn test_enforce_read_scope_allows_agent_dir_outside_cwd() {
        // Skills, prompt templates, and themes live under the agent dir
        // (`~/.pi/agent/` by default). The agent legitimately needs to read
        // these even when cwd is a user project directory on a different
        // path. Verify enforce_read_scope_with_agent_dir accepts a file
        // under the agent dir even when it is NOT a descendant of cwd.
        let cwd = tempfile::tempdir().unwrap();
        let agent_dir = tempfile::tempdir().unwrap();
        let skill_dir = agent_dir.path().join("skills").join("example");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let skill_path = skill_dir.join("SKILL.md");
        std::fs::write(&skill_path, "---\nname: example\n---\nhi\n").unwrap();

        let result = enforce_read_scope_with_agent_dir(&skill_path, cwd.path(), agent_dir.path());
        assert!(
            result.is_ok(),
            "expected read scope to allow file under agent dir, got: {result:?}"
        );
    }

    #[test]
    fn test_enforce_read_scope_still_rejects_unrelated_paths() {
        // Sanity check: the new agent-dir allowance is additive. Paths
        // outside BOTH cwd and agent_dir must still be rejected.
        let cwd = tempfile::tempdir().unwrap();
        let agent_dir = tempfile::tempdir().unwrap();
        let unrelated = tempfile::tempdir().unwrap();
        std::fs::write(unrelated.path().join("secret.txt"), "secret").unwrap();
        let secret_path = unrelated.path().join("secret.txt");

        let err = enforce_read_scope_with_agent_dir(&secret_path, cwd.path(), agent_dir.path())
            .unwrap_err();
        assert!(
            err.to_string().contains("outside the working directory"),
            "expected validation error, got: {err}"
        );
    }

    #[test]
    fn test_read_empty_file() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::write(tmp.path().join("empty.txt"), "").unwrap();

            let tool = ReadTool::new(tmp.path());
            let out = tool
                .execute(
                    "t",
                    serde_json::json!({ "path": tmp.path().join("empty.txt").to_string_lossy() }),
                    None,
                )
                .await
                .unwrap();
            let text = get_text(&out.content);
            assert_eq!(text, "");
            assert!(!out.is_error);
        });
    }

    #[test]
    fn test_read_empty_file_positive_offset_errors() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::write(tmp.path().join("empty.txt"), "").unwrap();

            let tool = ReadTool::new(tmp.path());
            let err = tool
                .execute(
                    "t",
                    serde_json::json!({
                        "path": tmp.path().join("empty.txt").to_string_lossy(),
                        "offset": 1
                    }),
                    None,
                )
                .await;
            assert!(err.is_err());
            let msg = err.unwrap_err().to_string();
            assert!(msg.contains("beyond end of file"));
        });
    }

    #[test]
    fn test_read_rejects_zero_limit() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::write(tmp.path().join("lines.txt"), "a\nb\nc\n").unwrap();

            let tool = ReadTool::new(tmp.path());
            let err = tool
                .execute(
                    "t",
                    serde_json::json!({
                        "path": tmp.path().join("lines.txt").to_string_lossy(),
                        "limit": 0
                    }),
                    None,
                )
                .await;
            assert!(err.is_err());
            assert!(
                err.unwrap_err()
                    .to_string()
                    .contains("`limit` must be greater than 0")
            );
        });
    }

    #[test]
    fn test_read_offset_and_limit() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::write(
                tmp.path().join("lines.txt"),
                "L1\nL2\nL3\nL4\nL5\nL6\nL7\nL8\nL9\nL10",
            )
            .unwrap();

            let tool = ReadTool::new(tmp.path());
            let out = tool
                .execute(
                    "t",
                    serde_json::json!({
                        "path": tmp.path().join("lines.txt").to_string_lossy(),
                        "offset": 3,
                        "limit": 2
                    }),
                    None,
                )
                .await
                .unwrap();
            let text = get_text(&out.content);
            assert!(text.contains("L3"));
            assert!(text.contains("L4"));
            assert!(!text.contains("L2"));
            assert!(!text.contains("L5"));
        });
    }

    #[test]
    fn test_read_offset_and_limit_with_cr_only_line_endings() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::write(tmp.path().join("lines.txt"), b"L1\rL2\rL3\r").unwrap();

            let tool = ReadTool::new(tmp.path());
            let out = tool
                .execute(
                    "t",
                    serde_json::json!({
                        "path": tmp.path().join("lines.txt").to_string_lossy(),
                        "offset": 2,
                        "limit": 1
                    }),
                    None,
                )
                .await
                .unwrap();
            let text = get_text(&out.content);
            assert!(text.contains("L2"));
            assert!(!text.contains("L1"));
            assert!(!text.contains("L3"));
            assert!(text.contains("offset=3"));
            assert!(!text.contains('\r'));
        });
    }

    #[test]
    fn test_read_offset_and_limit_with_split_crlf_chunk_boundary() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            let mut content = vec![b'x'; (64 * 1024) - 1];
            content.extend_from_slice(b"\r\nSECOND\r\nTHIRD");
            std::fs::write(tmp.path().join("lines.txt"), content).unwrap();

            let tool = ReadTool::new(tmp.path());
            let out = tool
                .execute(
                    "t",
                    serde_json::json!({
                        "path": tmp.path().join("lines.txt").to_string_lossy(),
                        "offset": 2,
                        "limit": 1
                    }),
                    None,
                )
                .await
                .unwrap();
            let text = get_text(&out.content);
            assert!(text.contains("SECOND"));
            assert!(!text.contains("THIRD"));
            assert!(!text.contains("xxxx"));
            assert!(text.contains("offset=3"));
        });
    }

    #[test]
    fn test_read_offset_beyond_eof() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::write(tmp.path().join("short.txt"), "a\nb").unwrap();

            let tool = ReadTool::new(tmp.path());
            let err = tool
                .execute(
                    "t",
                    serde_json::json!({
                        "path": tmp.path().join("short.txt").to_string_lossy(),
                        "offset": 100
                    }),
                    None,
                )
                .await;
            assert!(err.is_err());
            let msg = err.unwrap_err().to_string();
            assert!(msg.contains("beyond end of file"));
        });
    }

    #[test]
    fn test_map_normalized_with_trailing_whitespace() {
        // "A   \nB" -> "A\nB" (normalized strips trailing spaces)
        let content = "A   \nB";
        let normalized = build_normalized_content(content);
        assert_eq!(normalized, "A\nB");

        // Find "A" (norm idx 0)
        let (start, len) = map_normalized_range_to_original(content, 0, 1);
        assert_eq!(start, 0);
        assert_eq!(len, 1);
        assert_eq!(&content[start..start + len], "A");

        // Find "\n" (norm idx 1)
        let (start, len) = map_normalized_range_to_original(content, 1, 1);
        assert_eq!(start, 4);
        assert_eq!(len, 1);
        assert_eq!(&content[start..start + len], "\n");

        // Find "B" (norm idx 2)
        let (start, len) = map_normalized_range_to_original(content, 2, 1);
        assert_eq!(start, 5);
        assert_eq!(len, 1);
        assert_eq!(&content[start..start + len], "B");
    }

    #[test]
    fn test_read_binary_file_lossy() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            let binary_data: Vec<u8> = (0..=255).collect();
            std::fs::write(tmp.path().join("binary.bin"), &binary_data).unwrap();

            let tool = ReadTool::new(tmp.path());
            let out = tool
                .execute(
                    "t",
                    serde_json::json!({ "path": tmp.path().join("binary.bin").to_string_lossy() }),
                    None,
                )
                .await
                .unwrap();
            // Binary files are read as lossy UTF-8 with replacement characters
            let text = get_text(&out.content);
            assert!(!text.is_empty());
            assert!(!out.is_error);
        });
    }

    #[test]
    fn test_read_image_detection() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            // Minimal valid PNG header
            let png_header: Vec<u8> = vec![
                0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // PNG signature
                0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, // IHDR chunk
                0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, // 1x1 pixel
                0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53,
                0xDE, // bit depth, color type, etc
                0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, // IDAT chunk
                0x08, 0xD7, 0x63, 0xF8, 0xCF, 0xC0, 0x00, 0x00, // compressed data
                0x00, 0x02, 0x00, 0x01, 0xE2, 0x21, 0xBC, 0x33, // CRC
                0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, // IEND chunk
                0xAE, 0x42, 0x60, 0x82,
            ];
            std::fs::write(tmp.path().join("test.png"), &png_header).unwrap();

            let tool = ReadTool::new(tmp.path());
            let out = tool
                .execute(
                    "t",
                    serde_json::json!({ "path": tmp.path().join("test.png").to_string_lossy() }),
                    None,
                )
                .await
                .unwrap();

            // Should return an image content block
            let has_image = out
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::Image(_)));
            assert!(has_image, "expected image content block for PNG file");
        });
    }

    #[cfg(feature = "image-resize")]
    #[test]
    fn test_read_resizes_large_source_image_before_api_limit_check() {
        asupersync::test_utils::run_test(|| async {
            use image::codecs::png::PngEncoder;
            use image::{ExtendedColorType, ImageEncoder, Rgb, RgbImage};

            let tmp = tempfile::tempdir().unwrap();
            let image = RgbImage::from_fn(2600, 2600, |x, y| {
                let seed = x.wrapping_mul(1_973)
                    ^ y.wrapping_mul(9_277)
                    ^ x.rotate_left(7)
                    ^ y.rotate_left(13);
                Rgb([
                    u8::try_from(seed % 256).unwrap_or(0),
                    u8::try_from((seed >> 8) % 256).unwrap_or(0),
                    u8::try_from((seed >> 16) % 256).unwrap_or(0),
                ])
            });

            let mut png_bytes = Vec::new();
            PngEncoder::new(&mut png_bytes)
                .write_image(
                    image.as_raw(),
                    image.width(),
                    image.height(),
                    ExtendedColorType::Rgb8,
                )
                .unwrap();

            assert!(
                png_bytes.len() > IMAGE_MAX_BYTES,
                "fixture must exceed API image limit to exercise resize path"
            );
            assert!(
                png_bytes.len() < usize::try_from(READ_TOOL_MAX_BYTES).unwrap_or(usize::MAX),
                "fixture must stay within read-tool input bound"
            );

            let image_path = tmp.path().join("large.png");
            std::fs::write(&image_path, &png_bytes).unwrap();

            let tool = ReadTool::new(tmp.path());
            let out = tool
                .execute(
                    "t",
                    serde_json::json!({ "path": image_path.to_string_lossy() }),
                    None,
                )
                .await
                .unwrap();

            assert!(!out.is_error, "resizable large images should succeed");
            assert!(
                out.content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::Image(_))),
                "expected an image attachment after resizing"
            );

            let text = get_text(&out.content);
            assert!(text.contains("Read image file"));
            assert!(
                text.contains("displayed at"),
                "expected resize note in read output, got: {text}"
            );
        });
    }

    #[test]
    fn test_read_blocked_images() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            let png_header: Vec<u8> =
                vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00];
            std::fs::write(tmp.path().join("test.png"), &png_header).unwrap();

            let tool = ReadTool::with_settings(tmp.path(), false, true);
            let err = tool
                .execute(
                    "t",
                    serde_json::json!({ "path": tmp.path().join("test.png").to_string_lossy() }),
                    None,
                )
                .await;
            assert!(err.is_err());
            assert!(err.unwrap_err().to_string().contains("blocked"));
        });
    }

    #[test]
    fn test_read_truncation_at_max_lines() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            let content: String = (0..DEFAULT_MAX_LINES + 500)
                .map(|i| format!("line {i}"))
                .collect::<Vec<_>>()
                .join("\n");
            std::fs::write(tmp.path().join("big.txt"), &content).unwrap();

            let tool = ReadTool::new(tmp.path());
            let out = tool
                .execute(
                    "t",
                    serde_json::json!({ "path": tmp.path().join("big.txt").to_string_lossy() }),
                    None,
                )
                .await
                .unwrap();
            // Should have truncation details
            assert!(out.details.is_some(), "expected truncation details");
            let text = get_text(&out.content);
            assert!(text.contains("offset="));
        });
    }

    #[test]
    fn test_read_first_line_exceeds_max_bytes() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            let long_line = "a".repeat(DEFAULT_MAX_BYTES + 128);
            std::fs::write(tmp.path().join("too_long.txt"), long_line).unwrap();

            let tool = ReadTool::new(tmp.path());
            let out = tool
                .execute(
                    "t",
                    serde_json::json!({ "path": tmp.path().join("too_long.txt").to_string_lossy() }),
                    None,
                )
                .await
                .unwrap();

            let text = get_text(&out.content);
            let expected_limit = format!("exceeds {} limit", format_size(DEFAULT_MAX_BYTES));
            assert!(
                text.contains(&expected_limit),
                "expected limit hint '{expected_limit}', got: {text}"
            );
            let details = out.details.expect("expected truncation details");
            assert_eq!(
                details
                    .get("truncation")
                    .and_then(|v| v.get("firstLineExceedsLimit"))
                    .and_then(serde_json::Value::as_bool),
                Some(true)
            );
        });
    }

    #[test]
    fn test_read_unicode_content() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::write(tmp.path().join("uni.txt"), "Hello 你好 🌍\nLine 2 café").unwrap();

            let tool = ReadTool::new(tmp.path());
            let out = tool
                .execute(
                    "t",
                    serde_json::json!({ "path": tmp.path().join("uni.txt").to_string_lossy() }),
                    None,
                )
                .await
                .unwrap();
            let text = get_text(&out.content);
            assert!(text.contains("你好"));
            assert!(text.contains("🌍"));
            assert!(text.contains("café"));
        });
    }

    // ========================================================================
    // Write Tool Tests
    // ========================================================================

    #[test]
    fn test_write_new_file() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            let tool = WriteTool::new(tmp.path());
            let out = tool
                .execute(
                    "t",
                    serde_json::json!({
                        "path": tmp.path().join("new.txt").to_string_lossy(),
                        "content": "hello world"
                    }),
                    None,
                )
                .await
                .unwrap();
            assert!(!out.is_error);
            let contents = std::fs::read_to_string(tmp.path().join("new.txt")).unwrap();
            assert_eq!(contents, "hello world");
        });
    }

    #[test]
    fn test_write_overwrite_existing() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::write(tmp.path().join("exist.txt"), "old content").unwrap();

            let tool = WriteTool::new(tmp.path());
            let out = tool
                .execute(
                    "t",
                    serde_json::json!({
                        "path": tmp.path().join("exist.txt").to_string_lossy(),
                        "content": "new content"
                    }),
                    None,
                )
                .await
                .unwrap();
            assert!(!out.is_error);
            let contents = std::fs::read_to_string(tmp.path().join("exist.txt")).unwrap();
            assert_eq!(contents, "new content");
        });
    }

    #[test]
    fn test_write_creates_parent_dirs() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            let tool = WriteTool::new(tmp.path());
            let deep_path = tmp.path().join("a/b/c/deep.txt");
            let out = tool
                .execute(
                    "t",
                    serde_json::json!({
                        "path": deep_path.to_string_lossy(),
                        "content": "deep file"
                    }),
                    None,
                )
                .await
                .unwrap();
            assert!(!out.is_error);
            assert!(deep_path.exists());
            assert_eq!(std::fs::read_to_string(&deep_path).unwrap(), "deep file");
        });
    }

    #[test]
    fn test_write_empty_file() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            let tool = WriteTool::new(tmp.path());
            let out = tool
                .execute(
                    "t",
                    serde_json::json!({
                        "path": tmp.path().join("empty.txt").to_string_lossy(),
                        "content": ""
                    }),
                    None,
                )
                .await
                .unwrap();
            assert!(!out.is_error);
            let contents = std::fs::read_to_string(tmp.path().join("empty.txt")).unwrap();
            assert_eq!(contents, "");
            let text = get_text(&out.content);
            assert!(text.contains("Successfully wrote 0 bytes"));
        });
    }

    #[test]
    fn test_write_rejects_outside_cwd() {
        asupersync::test_utils::run_test(|| async {
            let cwd = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            let tool = WriteTool::new(cwd.path());
            let err = tool
                .execute(
                    "t",
                    serde_json::json!({
                        "path": outside.path().join("escape.txt").to_string_lossy(),
                        "content": "nope"
                    }),
                    None,
                )
                .await
                .unwrap_err();
            assert!(err.to_string().contains("outside the working directory"));

            let err = tool
                .execute(
                    "t",
                    serde_json::json!({
                        "path": "../escape.txt",
                        "content": "nope"
                    }),
                    None,
                )
                .await
                .unwrap_err();
            assert!(err.to_string().contains("outside the working directory"));
        });
    }

    #[test]
    fn test_write_unicode_content() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            let tool = WriteTool::new(tmp.path());
            let out = tool
                .execute(
                    "t",
                    serde_json::json!({
                        "path": tmp.path().join("unicode.txt").to_string_lossy(),
                        "content": "日本語 🎉 Ñoño"
                    }),
                    None,
                )
                .await
                .unwrap();
            assert!(!out.is_error);
            let contents = std::fs::read_to_string(tmp.path().join("unicode.txt")).unwrap();
            assert_eq!(contents, "日本語 🎉 Ñoño");
        });
    }

    #[test]
    #[cfg(unix)]
    fn test_write_file_permissions_unix() {
        use std::os::unix::fs::PermissionsExt;
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            let tool = WriteTool::new(tmp.path());
            let path = tmp.path().join("perms.txt");
            let out = tool
                .execute(
                    "t",
                    serde_json::json!({
                        "path": path.to_string_lossy(),
                        "content": "check perms"
                    }),
                    None,
                )
                .await
                .unwrap();
            assert!(!out.is_error);

            let meta = std::fs::metadata(&path).unwrap();
            let mode = meta.permissions().mode();
            assert_eq!(
                mode & 0o777,
                0o644,
                "Expected default 0o644 permissions for new files"
            );
        });
    }

    // ========================================================================
    // Edit Tool Tests
    // ========================================================================

    #[test]
    fn test_edit_exact_match_replace() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::write(tmp.path().join("code.rs"), "fn foo() { bar() }").unwrap();

            let tool = EditTool::new(tmp.path());
            let out = tool
                .execute(
                    "t",
                    serde_json::json!({
                        "path": tmp.path().join("code.rs").to_string_lossy(),
                        "oldText": "bar()",
                        "newText": "baz()"
                    }),
                    None,
                )
                .await
                .unwrap();
            assert!(!out.is_error);
            let contents = std::fs::read_to_string(tmp.path().join("code.rs")).unwrap();
            assert_eq!(contents, "fn foo() { baz() }");
        });
    }

    #[test]
    fn test_edit_no_match_error() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::write(tmp.path().join("code.rs"), "fn foo() {}").unwrap();

            let tool = EditTool::new(tmp.path());
            let err = tool
                .execute(
                    "t",
                    serde_json::json!({
                        "path": tmp.path().join("code.rs").to_string_lossy(),
                        "oldText": "NONEXISTENT TEXT",
                        "newText": "replacement"
                    }),
                    None,
                )
                .await;
            assert!(err.is_err());
        });
    }

    #[test]
    fn test_edit_empty_old_text_error() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("code.rs");
            std::fs::write(&path, "fn foo() {}").unwrap();

            let tool = EditTool::new(tmp.path());
            let err = tool
                .execute(
                    "t",
                    serde_json::json!({
                        "path": path.to_string_lossy(),
                        "oldText": "",
                        "newText": "prefix"
                    }),
                    None,
                )
                .await
                .expect_err("empty oldText should be rejected");

            let msg = err.to_string();
            assert!(
                msg.contains("old text cannot be empty"),
                "unexpected error: {msg}"
            );
            let after = std::fs::read_to_string(path).unwrap();
            assert_eq!(after, "fn foo() {}");
        });
    }

    #[test]
    fn test_edit_ambiguous_match_error() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::write(tmp.path().join("dup.txt"), "hello hello hello").unwrap();

            let tool = EditTool::new(tmp.path());
            let err = tool
                .execute(
                    "t",
                    serde_json::json!({
                        "path": tmp.path().join("dup.txt").to_string_lossy(),
                        "oldText": "hello",
                        "newText": "world"
                    }),
                    None,
                )
                .await;
            assert!(err.is_err(), "expected error for ambiguous match");
        });
    }

    #[test]
    fn test_edit_multi_line_replacement() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::write(
                tmp.path().join("multi.txt"),
                "line 1\nline 2\nline 3\nline 4",
            )
            .unwrap();

            let tool = EditTool::new(tmp.path());
            let out = tool
                .execute(
                    "t",
                    serde_json::json!({
                        "path": tmp.path().join("multi.txt").to_string_lossy(),
                        "oldText": "line 2\nline 3",
                        "newText": "replaced 2\nreplaced 3\nextra line"
                    }),
                    None,
                )
                .await
                .unwrap();
            assert!(!out.is_error);
            let contents = std::fs::read_to_string(tmp.path().join("multi.txt")).unwrap();
            assert_eq!(
                contents,
                "line 1\nreplaced 2\nreplaced 3\nextra line\nline 4"
            );
        });
    }

    #[test]
    fn test_edit_unicode_content() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::write(tmp.path().join("uni.txt"), "Héllo wörld 🌍").unwrap();

            let tool = EditTool::new(tmp.path());
            let out = tool
                .execute(
                    "t",
                    serde_json::json!({
                        "path": tmp.path().join("uni.txt").to_string_lossy(),
                        "oldText": "wörld 🌍",
                        "newText": "Welt 🌎"
                    }),
                    None,
                )
                .await
                .unwrap();
            assert!(!out.is_error);
            let contents = std::fs::read_to_string(tmp.path().join("uni.txt")).unwrap();
            assert_eq!(contents, "Héllo Welt 🌎");
        });
    }

    #[test]
    fn test_edit_missing_file() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            let tool = EditTool::new(tmp.path());
            let err = tool
                .execute(
                    "t",
                    serde_json::json!({
                        "path": tmp.path().join("nope.txt").to_string_lossy(),
                        "oldText": "foo",
                        "newText": "bar"
                    }),
                    None,
                )
                .await;
            assert!(err.is_err());
        });
    }

    // ========================================================================
    // Bash Tool Tests
    // ========================================================================

    struct FailingReader {
        responses: std::collections::VecDeque<std::io::Result<Vec<u8>>>,
    }

    impl FailingReader {
        fn new(responses: impl IntoIterator<Item = std::io::Result<Vec<u8>>>) -> Self {
            Self {
                responses: responses.into_iter().collect(),
            }
        }
    }

    impl Read for FailingReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            match self.responses.pop_front().unwrap_or_else(|| Ok(Vec::new())) {
                Ok(bytes) => {
                    assert!(
                        bytes.len() <= buf.len(),
                        "test reader only supports single-chunk reads"
                    );
                    buf[..bytes.len()].copy_from_slice(&bytes);
                    Ok(bytes.len())
                }
                Err(err) => Err(err),
            }
        }
    }

    #[test]
    fn test_bash_simple_command() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            let tool = BashTool::new(tmp.path());
            let out = tool
                .execute(
                    "t",
                    serde_json::json!({ "command": "echo hello_from_bash" }),
                    None,
                )
                .await
                .unwrap();
            let text = get_text(&out.content);
            assert!(text.contains("hello_from_bash"));
            assert!(!out.is_error);
        });
    }

    #[test]
    fn test_bash_exit_code_nonzero() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            let tool = BashTool::new(tmp.path());
            let out = tool
                .execute("t", serde_json::json!({ "command": "exit 42" }), None)
                .await
                .expect("non-zero exit should return Ok with is_error=true");
            assert!(out.is_error, "non-zero exit must set is_error");
            let msg = get_text(&out.content);
            assert!(
                msg.contains("42"),
                "expected exit code 42 in output, got: {msg}"
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_bash_signal_termination_is_error() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            let tool = BashTool::new(tmp.path());
            let out = tool
                .execute("t", serde_json::json!({ "command": "kill -KILL $$" }), None)
                .await
                .expect("signal-terminated shell should return Ok with is_error=true");
            assert!(
                out.is_error,
                "signal-terminated shell must be reported as error"
            );
            let msg = get_text(&out.content);
            assert!(
                msg.contains("Command exited with code"),
                "expected explicit exit-code report, got: {msg}"
            );
            assert!(
                !msg.contains("Command exited with code 0"),
                "signal-terminated shell must not appear successful: {msg}"
            );
        });
    }

    #[test]
    fn test_bash_stderr_capture() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            let tool = BashTool::new(tmp.path());
            let out = tool
                .execute(
                    "t",
                    serde_json::json!({ "command": "echo stderr_msg >&2" }),
                    None,
                )
                .await
                .unwrap();
            let text = get_text(&out.content);
            assert!(
                text.contains("stderr_msg"),
                "expected stderr output in result, got: {text}"
            );
        });
    }

    #[test]
    fn test_bash_timeout() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            let tool = BashTool::new(tmp.path());
            let out = tool
                .execute(
                    "t",
                    serde_json::json!({ "command": "sleep 60", "timeout": 2 }),
                    None,
                )
                .await
                .expect("timeout should return Ok with is_error=true");
            assert!(out.is_error, "timeout must set is_error");
            let msg = get_text(&out.content);
            assert!(
                msg.to_lowercase().contains("timeout") || msg.to_lowercase().contains("timed out"),
                "expected timeout indication, got: {msg}"
            );
        });
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_bash_timeout_kills_process_tree() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            let marker = tmp.path().join("leaked_child.txt");
            let tool = BashTool::new(tmp.path());

            let out = tool
                .execute(
                    "t",
                    serde_json::json!({
                        "command": "(sleep 3; echo leaked > leaked_child.txt) & sleep 10",
                        "timeout": 1
                    }),
                    None,
                )
                .await
                .expect("timeout should return Ok with is_error=true");

            assert!(out.is_error, "timeout must set is_error");
            let msg = get_text(&out.content);
            assert!(msg.contains("Command timed out"));

            // If process tree cleanup fails, this file appears after ~3 seconds.
            std::thread::sleep(Duration::from_secs(4));
            assert!(
                !marker.exists(),
                "background child was not terminated on timeout"
            );
        });
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_bash_cancelled_context_kills_process_tree() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            let marker = tmp.path().join("leaked_child.txt");

            let ambient_cx = asupersync::Cx::for_testing();
            let cancel_cx = ambient_cx.clone();
            let _current = asupersync::Cx::set_current(Some(ambient_cx));

            let cancel_thread = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(100));
                cancel_cx.set_cancel_requested(true);
            });

            let result = run_bash_command(
                tmp.path(),
                None,
                None,
                "(sleep 3; echo leaked > leaked_child.txt) & sleep 10",
                Some(30),
                None,
            )
            .await
            .expect("cancelled bash should return a result");

            cancel_thread.join().expect("cancel thread");

            assert!(
                result.cancelled,
                "expected cancelled bash result: {result:?}"
            );

            std::thread::sleep(Duration::from_secs(4));
            assert!(
                !marker.exists(),
                "background child was not terminated on cancellation"
            );
        });
    }

    #[test]
    fn test_bash_pump_stream_emits_io_error_frame_after_partial_output() {
        let reader = FailingReader::new([
            Ok(b"partial stdout".to_vec()),
            Err(std::io::Error::other("simulated stdout failure")),
        ]);
        let (tx, rx) = mpsc::sync_channel::<BashPipeFrame>(4);

        pump_stream(reader, "stdout", &tx);

        match rx.recv().expect("partial chunk") {
            BashPipeFrame::Chunk(chunk) => assert_eq!(chunk, b"partial stdout"),
            BashPipeFrame::Error(message) => {
                unreachable!("expected output chunk before error, got error frame: {message}")
            }
        }

        match rx.recv().expect("io error frame") {
            BashPipeFrame::Chunk(chunk) => {
                unreachable!("expected io error after partial chunk, got chunk: {chunk:?}")
            }
            BashPipeFrame::Error(message) => {
                assert!(message.contains("Failed to read bash stdout"));
                assert!(message.contains("simulated stdout failure"));
            }
        }

        assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
    }

    #[test]
    fn test_drain_bash_output_ignores_cancellation_after_process_exit() {
        asupersync::test_utils::run_test(|| async {
            let (tx, mut rx) = mpsc::sync_channel::<BashPipeFrame>(1);
            let mut bash_output = BashOutputState::new(DEFAULT_MAX_BYTES);

            let ambient_cx = asupersync::Cx::for_testing();
            ambient_cx.set_cancel_requested(true);
            let _current = asupersync::Cx::set_current(Some(ambient_cx));
            let cx = AgentCx::for_current_or_request();
            let now = cx
                .cx()
                .timer_driver()
                .map_or_else(wall_now, |timer| timer.now());

            let cancelled = drain_bash_output(
                &mut rx,
                &mut bash_output,
                &cx,
                now + std::time::Duration::from_millis(10),
                std::time::Duration::from_millis(1),
                false,
            )
            .await
            .expect("drain should complete without cancellation");

            drop(tx);

            assert!(
                !cancelled,
                "post-exit drain should ignore late ambient cancellation"
            );
            assert_eq!(bash_output.total_bytes, 0);
        });
    }

    #[test]
    fn test_drain_bash_output_returns_pipe_read_error() {
        asupersync::test_utils::run_test(|| async {
            let (tx, mut rx) = mpsc::sync_channel::<BashPipeFrame>(2);
            tx.send(BashPipeFrame::Chunk(b"partial stderr".to_vec()))
                .expect("queue partial output");
            tx.send(BashPipeFrame::Error(
                "Failed to read bash stderr: simulated stderr failure".to_string(),
            ))
            .expect("queue error frame");
            drop(tx);

            let mut bash_output = BashOutputState::new(DEFAULT_MAX_BYTES);
            let cx = AgentCx::for_current_or_request();
            let now = cx
                .cx()
                .timer_driver()
                .map_or_else(wall_now, |timer| timer.now());

            let err = drain_bash_output(
                &mut rx,
                &mut bash_output,
                &cx,
                now + std::time::Duration::from_millis(10),
                std::time::Duration::from_millis(1),
                false,
            )
            .await
            .expect_err("pipe read failures must surface as errors");

            let message = err.to_string();
            assert!(message.contains("Failed to read bash stderr"));
            assert!(message.contains("simulated stderr failure"));
            assert!(message.contains("Partial output before failure"));
            assert!(message.contains("partial stderr"));
            assert_eq!(bash_output.total_bytes, "partial stderr".len());
        });
    }

    #[test]
    fn test_drain_bash_output_honors_cancellation_while_process_still_active() {
        asupersync::test_utils::run_test(|| async {
            let (_tx, mut rx) = mpsc::sync_channel::<BashPipeFrame>(1);
            let mut bash_output = BashOutputState::new(DEFAULT_MAX_BYTES);

            let ambient_cx = asupersync::Cx::for_testing();
            ambient_cx.set_cancel_requested(true);
            let _current = asupersync::Cx::set_current(Some(ambient_cx));
            let cx = AgentCx::for_current_or_request();
            let now = cx
                .cx()
                .timer_driver()
                .map_or_else(wall_now, |timer| timer.now());

            let cancelled = drain_bash_output(
                &mut rx,
                &mut bash_output,
                &cx,
                now + std::time::Duration::from_secs(1),
                std::time::Duration::from_millis(1),
                true,
            )
            .await
            .expect("drain should complete under cancellation");

            assert!(
                cancelled,
                "active drain should still honor ambient cancellation"
            );
            assert_eq!(bash_output.total_bytes, 0);
        });
    }

    #[test]
    fn test_bash_output_state_abandon_spill_file_clears_path_and_unlinks_file() {
        let tmp = tempfile::tempdir().unwrap();
        let spill_path = tmp.path().join("partial-bash.log");
        std::fs::write(&spill_path, b"partial output").unwrap();

        let mut bash_output = BashOutputState::new(DEFAULT_MAX_BYTES);
        bash_output.temp_file_path = Some(spill_path.clone());

        bash_output.abandon_spill_file();

        assert!(bash_output.spill_failed);
        assert!(bash_output.temp_file.is_none());
        assert!(bash_output.temp_file_path.is_none());
        assert!(
            !spill_path.exists(),
            "abandoned spill files should not be advertised or left behind"
        );
    }

    #[test]
    fn test_bash_hard_limit_retains_partial_spill_file() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            let spill_path = tmp.path().join("hard-limit-bash.log");
            std::fs::write(&spill_path, b"partial output").unwrap();

            let spill_file = asupersync::fs::OpenOptions::new()
                .append(true)
                .open(&spill_path)
                .await
                .unwrap();

            let mut bash_output = BashOutputState::new(DEFAULT_MAX_BYTES);
            bash_output.total_bytes = BASH_FILE_LIMIT_BYTES;
            bash_output.temp_file_path = Some(spill_path.clone());
            bash_output.temp_file = Some(spill_file);

            ingest_bash_chunk(vec![b'x'], &mut bash_output)
                .await
                .expect("hard-limit ingestion should still succeed");

            assert!(!bash_output.spill_failed);
            assert!(bash_output.temp_file.is_none());
            assert!(bash_output.temp_file_path.is_some());
            assert!(
                spill_path.exists(),
                "partial spill files must be retained once the hard limit is reached for diagnostics"
            );
        });
    }

    #[test]
    #[cfg(unix)]
    fn test_bash_working_directory() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            let tool = BashTool::new(tmp.path());
            let out = tool
                .execute("t", serde_json::json!({ "command": "pwd" }), None)
                .await
                .unwrap();
            let text = get_text(&out.content);
            let canonical = tmp.path().canonicalize().unwrap();
            assert!(
                text.contains(&canonical.to_string_lossy().to_string()),
                "expected cwd in output, got: {text}"
            );
        });
    }

    #[test]
    fn test_bash_multiline_output() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            let tool = BashTool::new(tmp.path());
            let out = tool
                .execute(
                    "t",
                    serde_json::json!({ "command": "echo line1; echo line2; echo line3" }),
                    None,
                )
                .await
                .unwrap();
            let text = get_text(&out.content);
            assert!(text.contains("line1"));
            assert!(text.contains("line2"));
            assert!(text.contains("line3"));
        });
    }

    // ========================================================================
    // Grep Tool Tests
    // ========================================================================

    #[test]
    fn test_grep_basic_pattern() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::write(
                tmp.path().join("search.txt"),
                "apple\nbanana\napricot\ncherry",
            )
            .unwrap();

            let tool = GrepTool::new(tmp.path());
            let out = tool
                .execute(
                    "t",
                    serde_json::json!({
                        "pattern": "ap",
                        "path": tmp.path().join("search.txt").to_string_lossy()
                    }),
                    None,
                )
                .await
                .unwrap();
            let text = get_text(&out.content);
            assert!(text.contains("apple"));
            assert!(text.contains("apricot"));
            assert!(!text.contains("banana"));
            assert!(!text.contains("cherry"));
        });
    }

    #[test]
    fn test_grep_rejects_outside_cwd() {
        asupersync::test_utils::run_test(|| async {
            let cwd = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            std::fs::write(outside.path().join("secret.txt"), "secret").unwrap();

            let tool = GrepTool::new(cwd.path());
            let err = tool
                .execute(
                    "t",
                    serde_json::json!({
                        "pattern": "secret",
                        "path": outside.path().join("secret.txt").to_string_lossy()
                    }),
                    None,
                )
                .await
                .unwrap_err();
            assert!(err.to_string().contains("outside the working directory"));
        });
    }

    #[test]
    fn test_grep_rejects_zero_limit() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::write(tmp.path().join("search.txt"), "alpha\nbeta\n").unwrap();

            let tool = GrepTool::new(tmp.path());
            let err = tool
                .execute(
                    "t",
                    serde_json::json!({
                        "pattern": "alpha",
                        "path": tmp.path().join("search.txt").to_string_lossy(),
                        "limit": 0
                    }),
                    None,
                )
                .await
                .unwrap_err();
            assert!(err.to_string().contains("`limit` must be greater than 0"));
        });
    }

    #[test]
    #[cfg(unix)]
    fn test_grep_formats_paths_relative_to_symlinked_cwd() {
        asupersync::test_utils::run_test(|| async {
            let real = tempfile::tempdir().unwrap();
            let link_parent = tempfile::tempdir().unwrap();
            let link = link_parent.path().join("linked-cwd");
            std::os::unix::fs::symlink(real.path(), &link).unwrap();
            std::fs::write(real.path().join("needle.txt"), "needle\n").unwrap();

            let tool = GrepTool::new(&link);
            let out = tool
                .execute("t", serde_json::json!({ "pattern": "needle" }), None)
                .await
                .unwrap();

            let text = get_text(&out.content);
            assert!(
                text.contains("needle.txt:1: needle"),
                "grep output should use cwd-relative paths for symlinked cwd, got: {text}"
            );
            assert!(
                !text.contains(real.path().to_string_lossy().as_ref()),
                "grep output should not leak canonical temp root, got: {text}"
            );
        });
    }

    #[test]
    fn test_grep_regex_pattern() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::write(
                tmp.path().join("regex.txt"),
                "foo123\nbar456\nbaz789\nfoo000",
            )
            .unwrap();

            let tool = GrepTool::new(tmp.path());
            let out = tool
                .execute(
                    "t",
                    serde_json::json!({
                        "pattern": "foo\\d+",
                        "path": tmp.path().join("regex.txt").to_string_lossy()
                    }),
                    None,
                )
                .await
                .unwrap();
            let text = get_text(&out.content);
            assert!(text.contains("foo123"));
            assert!(text.contains("foo000"));
            assert!(!text.contains("bar456"));
        });
    }

    #[test]
    fn test_grep_case_insensitive() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::write(tmp.path().join("case.txt"), "Hello\nhello\nHELLO").unwrap();

            let tool = GrepTool::new(tmp.path());
            let out = tool
                .execute(
                    "t",
                    serde_json::json!({
                        "pattern": "hello",
                        "path": tmp.path().join("case.txt").to_string_lossy(),
                        "ignoreCase": true
                    }),
                    None,
                )
                .await
                .unwrap();
            let text = get_text(&out.content);
            assert!(text.contains("Hello"));
            assert!(text.contains("hello"));
            assert!(text.contains("HELLO"));
        });
    }

    #[test]
    fn test_grep_case_sensitive_by_default() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::write(tmp.path().join("case_sensitive.txt"), "Hello\nHELLO").unwrap();

            let tool = GrepTool::new(tmp.path());
            let out = tool
                .execute(
                    "t",
                    serde_json::json!({
                        "pattern": "hello",
                        "path": tmp.path().join("case_sensitive.txt").to_string_lossy()
                    }),
                    None,
                )
                .await
                .unwrap();
            let text = get_text(&out.content);
            assert!(
                text.contains("No matches found"),
                "expected case-sensitive search to find no matches, got: {text}"
            );
        });
    }

    #[test]
    fn test_grep_append_non_matching_lines_invariant() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            let file = tmp.path().join("base.txt");
            std::fs::write(&file, "needle one\nskip\nneedle two\n").unwrap();

            let tool = GrepTool::new(tmp.path());
            let base_out = tool
                .execute(
                    "t",
                    serde_json::json!({
                        "pattern": "needle",
                        "path": file.to_string_lossy(),
                        "limit": 100
                    }),
                    None,
                )
                .await
                .unwrap();
            let base_text = get_text(&base_out.content);

            std::fs::write(&file, "needle one\nskip\nneedle two\nalpha\nbeta\n").unwrap();
            let extended_out = tool
                .execute(
                    "t",
                    serde_json::json!({
                        "pattern": "needle",
                        "path": file.to_string_lossy(),
                        "limit": 100
                    }),
                    None,
                )
                .await
                .unwrap();
            let extended_text = get_text(&extended_out.content);

            assert_eq!(
                base_text, extended_text,
                "adding non-matching lines should not alter grep output"
            );
        });
    }

    #[test]
    fn test_grep_no_matches() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::write(tmp.path().join("nothing.txt"), "alpha\nbeta\ngamma").unwrap();

            let tool = GrepTool::new(tmp.path());
            let out = tool
                .execute(
                    "t",
                    serde_json::json!({
                        "pattern": "ZZZZZ_NOMATCH",
                        "path": tmp.path().join("nothing.txt").to_string_lossy()
                    }),
                    None,
                )
                .await
                .unwrap();
            let text = get_text(&out.content);
            assert!(
                text.to_lowercase().contains("no match")
                    || text.is_empty()
                    || text.to_lowercase().contains("no results"),
                "expected no-match indication, got: {text}"
            );
        });
    }

    #[test]
    fn test_grep_context_lines() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::write(
                tmp.path().join("ctx.txt"),
                "aaa\nbbb\nccc\ntarget\nddd\neee\nfff",
            )
            .unwrap();

            let tool = GrepTool::new(tmp.path());
            let out = tool
                .execute(
                    "t",
                    serde_json::json!({
                        "pattern": "target",
                        "path": tmp.path().join("ctx.txt").to_string_lossy(),
                        "context": 1
                    }),
                    None,
                )
                .await
                .unwrap();
            let text = get_text(&out.content);
            assert!(text.contains("target"));
            assert!(text.contains("ccc"), "expected context line before match");
            assert!(text.contains("ddd"), "expected context line after match");
        });
    }

    #[test]
    fn test_grep_limit() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            let content: String = (0..200)
                .map(|i| format!("match_line_{i}"))
                .collect::<Vec<_>>()
                .join("\n");
            std::fs::write(tmp.path().join("many.txt"), &content).unwrap();

            let tool = GrepTool::new(tmp.path());
            let out = tool
                .execute(
                    "t",
                    serde_json::json!({
                        "pattern": "match_line",
                        "path": tmp.path().join("many.txt").to_string_lossy(),
                        "limit": 5
                    }),
                    None,
                )
                .await
                .unwrap();
            let text = get_text(&out.content);
            // With limit=5, we should see at most 5 matches
            let match_count = text.matches("match_line_").count();
            assert!(
                match_count <= 5,
                "expected at most 5 matches with limit=5, got {match_count}"
            );
            let details = out.details.expect("expected limit details");
            assert_eq!(
                details
                    .get("matchLimitReached")
                    .and_then(serde_json::Value::as_u64),
                Some(5)
            );
        });
    }

    #[test]
    fn test_grep_exact_limit_does_not_report_limit_reached() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            let content = (0..5)
                .map(|i| format!("match_line_{i}"))
                .collect::<Vec<_>>()
                .join("\n");
            std::fs::write(tmp.path().join("exact.txt"), &content).unwrap();

            let tool = GrepTool::new(tmp.path());
            let out = tool
                .execute(
                    "t",
                    serde_json::json!({
                        "pattern": "match_line",
                        "path": tmp.path().join("exact.txt").to_string_lossy(),
                        "limit": 5
                    }),
                    None,
                )
                .await
                .unwrap();

            let text = get_text(&out.content);
            assert_eq!(text.matches("match_line_").count(), 5);
            assert!(
                !text.contains("matches limit reached"),
                "exact-limit grep results should not claim truncation: {text}"
            );
            assert!(
                out.details
                    .as_ref()
                    .and_then(|details| details.get("matchLimitReached"))
                    .is_none(),
                "exact-limit grep results should not set matchLimitReached"
            );
        });
    }

    #[test]
    fn test_grep_large_output_does_not_deadlock_reader_threads() {
        asupersync::test_utils::run_test(|| async {
            use std::fmt::Write as _;

            let tmp = tempfile::tempdir().unwrap();
            let mut content = String::with_capacity(80_000);
            for i in 0..5000 {
                let _ = writeln!(&mut content, "needle_line_{i}");
            }
            let file = tmp.path().join("large_grep.txt");
            std::fs::write(&file, content).unwrap();

            let tool = GrepTool::new(tmp.path());
            let run = tool.execute(
                "t",
                serde_json::json!({
                    "pattern": "needle_line_",
                    "path": file.to_string_lossy(),
                    "limit": 6000
                }),
                None,
            );

            let out = asupersync::time::timeout(
                asupersync::time::wall_now(),
                Duration::from_secs(15),
                Box::pin(run),
            )
            .await
            .expect("grep timed out; possible stdout/stderr reader deadlock")
            .expect("grep should succeed");

            let text = get_text(&out.content);
            assert!(text.contains("needle_line_0"));
        });
    }

    #[test]
    fn test_grep_respects_gitignore() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::write(tmp.path().join(".gitignore"), "ignored.txt\n").unwrap();
            std::fs::write(tmp.path().join("ignored.txt"), "needle in ignored file").unwrap();
            std::fs::write(tmp.path().join("visible.txt"), "nothing here").unwrap();

            let tool = GrepTool::new(tmp.path());
            let out = tool
                .execute("t", serde_json::json!({ "pattern": "needle" }), None)
                .await
                .unwrap();

            let text = get_text(&out.content);
            assert!(
                text.contains("No matches found"),
                "expected ignored file to be excluded, got: {text}"
            );
        });
    }

    #[test]
    fn test_grep_literal_mode() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::write(tmp.path().join("literal.txt"), "a+b\na.b\nab\na\\+b").unwrap();

            let tool = GrepTool::new(tmp.path());
            let out = tool
                .execute(
                    "t",
                    serde_json::json!({
                        "pattern": "a+b",
                        "path": tmp.path().join("literal.txt").to_string_lossy(),
                        "literal": true
                    }),
                    None,
                )
                .await
                .unwrap();
            let text = get_text(&out.content);
            assert!(text.contains("a+b"), "literal match should find 'a+b'");
        });
    }

    #[test]
    fn test_grep_hashline_output() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::write(
                tmp.path().join("hash.txt"),
                "apple\nbanana\napricot\ncherry",
            )
            .unwrap();

            let tool = GrepTool::new(tmp.path());
            let out = tool
                .execute(
                    "t",
                    serde_json::json!({
                        "pattern": "ap",
                        "path": tmp.path().join("hash.txt").to_string_lossy(),
                        "hashline": true
                    }),
                    None,
                )
                .await
                .unwrap();
            let text = get_text(&out.content);
            // Hashline output should contain N#AB tags instead of bare line numbers
            // Line 1 (apple) and line 3 (apricot) should match
            assert!(text.contains("apple"), "should contain apple");
            assert!(text.contains("apricot"), "should contain apricot");
            assert!(
                !text.contains("banana"),
                "should not contain banana context"
            );
            // Verify hashline tag format: digit(s) followed by # and two uppercase letters
            let re = regex::Regex::new(r"\d+#[A-Z]{2}").unwrap();
            assert!(
                re.is_match(&text),
                "hashline output should contain N#AB tags, got: {text}"
            );
        });
    }

    #[test]
    fn test_grep_hashline_with_context() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::write(
                tmp.path().join("ctx.txt"),
                "line1\nline2\ntarget\nline4\nline5",
            )
            .unwrap();

            let tool = GrepTool::new(tmp.path());
            let out = tool
                .execute(
                    "t",
                    serde_json::json!({
                        "pattern": "target",
                        "path": tmp.path().join("ctx.txt").to_string_lossy(),
                        "hashline": true,
                        "context": 1
                    }),
                    None,
                )
                .await
                .unwrap();
            let text = get_text(&out.content);
            // With context=1, should include line2, target, line4
            assert!(text.contains("line2"), "should contain context line2");
            assert!(text.contains("target"), "should contain match");
            assert!(text.contains("line4"), "should contain context line4");
            // Match lines use `:` separator, context lines use `-`
            let re_match = regex::Regex::new(r"\d+#[A-Z]{2}: target").unwrap();
            assert!(
                re_match.is_match(&text),
                "match line should use : separator with hashline tag, got: {text}"
            );
            let re_ctx = regex::Regex::new(r"\d+#[A-Z]{2}- line").unwrap();
            assert!(
                re_ctx.is_match(&text),
                "context line should use - separator with hashline tag, got: {text}"
            );
        });
    }

    // ========================================================================
    // Find Tool Tests
    // ========================================================================

    #[test]
    fn test_find_glob_pattern() {
        asupersync::test_utils::run_test(|| async {
            if find_fd_binary().is_none() {
                return;
            }
            let tmp = tempfile::tempdir().unwrap();
            std::fs::write(tmp.path().join("file1.rs"), "").unwrap();
            std::fs::write(tmp.path().join("file2.rs"), "").unwrap();
            std::fs::write(tmp.path().join("file3.txt"), "").unwrap();

            let tool = FindTool::new(tmp.path());
            let out = tool
                .execute(
                    "t",
                    serde_json::json!({
                        "pattern": "*.rs",
                        "path": tmp.path().to_string_lossy()
                    }),
                    None,
                )
                .await
                .unwrap();
            let text = get_text(&out.content);
            assert!(text.contains("file1.rs"));
            assert!(text.contains("file2.rs"));
            assert!(!text.contains("file3.txt"));
        });
    }

    #[test]
    fn test_find_append_non_matching_file_invariant() {
        asupersync::test_utils::run_test(|| async {
            if find_fd_binary().is_none() {
                return;
            }
            let tmp = tempfile::tempdir().unwrap();
            std::fs::write(tmp.path().join("match.txt"), "a").unwrap();

            let tool = FindTool::new(tmp.path());
            let base_out = tool
                .execute(
                    "t",
                    serde_json::json!({
                        "pattern": "*.txt",
                        "path": tmp.path().to_string_lossy()
                    }),
                    None,
                )
                .await
                .unwrap();
            let base_text = get_text(&base_out.content);

            std::fs::write(tmp.path().join("ignore.md"), "b").unwrap();
            let extended_out = tool
                .execute(
                    "t",
                    serde_json::json!({
                        "pattern": "*.txt",
                        "path": tmp.path().to_string_lossy()
                    }),
                    None,
                )
                .await
                .unwrap();
            let extended_text = get_text(&extended_out.content);

            assert_eq!(
                base_text, extended_text,
                "adding non-matching files should not alter find output"
            );
        });
    }

    #[test]
    fn test_find_rejects_outside_cwd() {
        asupersync::test_utils::run_test(|| async {
            let cwd = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            std::fs::write(outside.path().join("secret.txt"), "secret").unwrap();

            let tool = FindTool::new(cwd.path());
            let err = tool
                .execute(
                    "t",
                    serde_json::json!({
                        "pattern": "*.txt",
                        "path": outside.path().to_string_lossy()
                    }),
                    None,
                )
                .await
                .unwrap_err();
            assert!(err.to_string().contains("outside the working directory"));
        });
    }

    #[test]
    fn test_find_limit() {
        asupersync::test_utils::run_test(|| async {
            if find_fd_binary().is_none() {
                return;
            }
            let tmp = tempfile::tempdir().unwrap();
            for i in 0..20 {
                std::fs::write(tmp.path().join(format!("f{i}.txt")), "").unwrap();
            }

            let tool = FindTool::new(tmp.path());
            let out = tool
                .execute(
                    "t",
                    serde_json::json!({
                        "pattern": "*.txt",
                        "path": tmp.path().to_string_lossy(),
                        "limit": 5
                    }),
                    None,
                )
                .await
                .unwrap();
            let text = get_text(&out.content);
            let file_count = text.lines().filter(|l| l.contains(".txt")).count();
            assert!(
                file_count <= 5,
                "expected at most 5 files with limit=5, got {file_count}"
            );
            let details = out.details.expect("expected limit details");
            assert_eq!(
                details
                    .get("resultLimitReached")
                    .and_then(serde_json::Value::as_u64),
                Some(5)
            );
        });
    }

    #[test]
    fn test_find_exact_limit_does_not_report_limit_reached() {
        asupersync::test_utils::run_test(|| async {
            if find_fd_binary().is_none() {
                return;
            }
            let tmp = tempfile::tempdir().unwrap();
            for i in 0..5 {
                std::fs::write(tmp.path().join(format!("f{i}.txt")), "").unwrap();
            }

            let tool = FindTool::new(tmp.path());
            let out = tool
                .execute(
                    "t",
                    serde_json::json!({
                        "pattern": "*.txt",
                        "path": tmp.path().to_string_lossy(),
                        "limit": 5
                    }),
                    None,
                )
                .await
                .unwrap();

            let text = get_text(&out.content);
            assert_eq!(text.lines().filter(|line| line.contains(".txt")).count(), 5);
            assert!(
                !text.contains("results limit reached"),
                "exact-limit find results should not claim truncation: {text}"
            );
            assert!(
                out.details
                    .as_ref()
                    .and_then(|details| details.get("resultLimitReached"))
                    .is_none(),
                "exact-limit find results should not set resultLimitReached"
            );
        });
    }

    #[test]
    fn test_find_zero_limit_is_rejected() {
        asupersync::test_utils::run_test(|| async {
            if find_fd_binary().is_none() {
                return;
            }
            let tmp = tempfile::tempdir().unwrap();
            std::fs::write(tmp.path().join("file.txt"), "").unwrap();

            let tool = FindTool::new(tmp.path());
            let err = tool
                .execute(
                    "t",
                    serde_json::json!({
                        "pattern": "*.txt",
                        "path": tmp.path().to_string_lossy(),
                        "limit": 0
                    }),
                    None,
                )
                .await
                .expect_err("limit=0 should be rejected");

            assert!(
                err.to_string().contains("`limit` must be greater than 0"),
                "expected validation error, got: {err}"
            );
        });
    }

    #[test]
    fn test_find_no_matches() {
        asupersync::test_utils::run_test(|| async {
            if find_fd_binary().is_none() {
                return;
            }
            let tmp = tempfile::tempdir().unwrap();
            std::fs::write(tmp.path().join("only.txt"), "").unwrap();

            let tool = FindTool::new(tmp.path());
            let out = tool
                .execute(
                    "t",
                    serde_json::json!({
                        "pattern": "*.rs",
                        "path": tmp.path().to_string_lossy()
                    }),
                    None,
                )
                .await
                .unwrap();
            let text = get_text(&out.content);
            assert!(
                text.to_lowercase().contains("no files found")
                    || text.to_lowercase().contains("no matches")
                    || text.is_empty(),
                "expected no-match indication, got: {text}"
            );
        });
    }

    #[test]
    fn test_find_nonexistent_path() {
        asupersync::test_utils::run_test(|| async {
            if find_fd_binary().is_none() {
                return;
            }
            let tmp = tempfile::tempdir().unwrap();
            let tool = FindTool::new(tmp.path());
            let err = tool
                .execute(
                    "t",
                    serde_json::json!({
                        "pattern": "*.rs",
                        "path": tmp.path().join("nonexistent").to_string_lossy()
                    }),
                    None,
                )
                .await;
            assert!(err.is_err());
        });
    }

    #[test]
    fn test_find_nested_directories() {
        asupersync::test_utils::run_test(|| async {
            if find_fd_binary().is_none() {
                return;
            }
            let tmp = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(tmp.path().join("a/b/c")).unwrap();
            std::fs::write(tmp.path().join("top.rs"), "").unwrap();
            std::fs::write(tmp.path().join("a/mid.rs"), "").unwrap();
            std::fs::write(tmp.path().join("a/b/c/deep.rs"), "").unwrap();

            let tool = FindTool::new(tmp.path());
            let out = tool
                .execute(
                    "t",
                    serde_json::json!({
                        "pattern": "*.rs",
                        "path": tmp.path().to_string_lossy()
                    }),
                    None,
                )
                .await
                .unwrap();
            let text = get_text(&out.content);
            assert!(text.contains("top.rs"));
            assert!(text.contains("mid.rs"));
            assert!(text.contains("deep.rs"));
        });
    }

    #[test]
    fn test_find_results_are_sorted() {
        // FindTool sorts by modification time (most recent first), then alphabetically
        // as a tie-breaker for files with the same mtime.
        asupersync::test_utils::run_test(|| async {
            if find_fd_binary().is_none() {
                return;
            }
            let tmp = tempfile::tempdir().unwrap();

            // Create files with delays to ensure distinct modification times.
            // Order: oldest first, so the expected output (most recent first) is reversed.
            std::fs::write(tmp.path().join("oldest.txt"), "").unwrap();
            std::thread::sleep(std::time::Duration::from_millis(50));
            std::fs::write(tmp.path().join("middle.txt"), "").unwrap();
            std::thread::sleep(std::time::Duration::from_millis(50));
            std::fs::write(tmp.path().join("newest.txt"), "").unwrap();

            let tool = FindTool::new(tmp.path());
            let out = tool
                .execute(
                    "t",
                    serde_json::json!({
                        "pattern": "*.txt",
                        "path": tmp.path().to_string_lossy()
                    }),
                    None,
                )
                .await
                .unwrap();
            let lines: Vec<String> = get_text(&out.content)
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_string)
                .collect();

            // Expected order: most recent first
            assert_eq!(
                lines,
                vec!["newest.txt", "middle.txt", "oldest.txt"],
                "expected mtime-sorted find output (most recent first)"
            );
        });
    }

    #[test]
    fn test_find_respects_gitignore() {
        asupersync::test_utils::run_test(|| async {
            if find_fd_binary().is_none() {
                return;
            }
            let tmp = tempfile::tempdir().unwrap();
            std::fs::write(tmp.path().join(".gitignore"), "ignored.txt\n").unwrap();
            std::fs::write(tmp.path().join("ignored.txt"), "").unwrap();

            let tool = FindTool::new(tmp.path());
            let out = tool
                .execute(
                    "t",
                    serde_json::json!({
                        "pattern": "*.txt",
                        "path": tmp.path().to_string_lossy()
                    }),
                    None,
                )
                .await
                .unwrap();
            let text = get_text(&out.content);
            assert!(
                text.contains("No files found matching pattern"),
                "expected .gitignore'd files to be excluded, got: {text}"
            );
        });
    }

    // ========================================================================
    // Ls Tool Tests
    // ========================================================================

    #[test]
    fn test_ls_directory_listing() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::write(tmp.path().join("file_a.txt"), "content").unwrap();
            std::fs::write(tmp.path().join("file_b.rs"), "fn main() {}").unwrap();
            std::fs::create_dir(tmp.path().join("subdir")).unwrap();

            let tool = LsTool::new(tmp.path());
            let out = tool
                .execute(
                    "t",
                    serde_json::json!({ "path": tmp.path().to_string_lossy() }),
                    None,
                )
                .await
                .unwrap();
            let text = get_text(&out.content);
            assert!(text.contains("file_a.txt"));
            assert!(text.contains("file_b.rs"));
            assert!(text.contains("subdir"));
        });
    }

    #[test]
    fn test_ls_rejects_outside_cwd() {
        asupersync::test_utils::run_test(|| async {
            let cwd = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            std::fs::write(outside.path().join("secret.txt"), "secret").unwrap();

            let tool = LsTool::new(cwd.path());
            let err = tool
                .execute(
                    "t",
                    serde_json::json!({ "path": outside.path().to_string_lossy() }),
                    None,
                )
                .await
                .unwrap_err();
            assert!(err.to_string().contains("outside the working directory"));
        });
    }

    #[test]
    fn test_ls_trailing_slash_for_dirs() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::write(tmp.path().join("file.txt"), "").unwrap();
            std::fs::create_dir(tmp.path().join("mydir")).unwrap();

            let tool = LsTool::new(tmp.path());
            let out = tool
                .execute(
                    "t",
                    serde_json::json!({ "path": tmp.path().to_string_lossy() }),
                    None,
                )
                .await
                .unwrap();
            let text = get_text(&out.content);
            assert!(
                text.contains("mydir/"),
                "expected trailing slash for directory, got: {text}"
            );
        });
    }

    #[test]
    fn test_ls_limit() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            for i in 0..20 {
                std::fs::write(tmp.path().join(format!("item_{i:02}.txt")), "").unwrap();
            }

            let tool = LsTool::new(tmp.path());
            let out = tool
                .execute(
                    "t",
                    serde_json::json!({
                        "path": tmp.path().to_string_lossy(),
                        "limit": 5
                    }),
                    None,
                )
                .await
                .unwrap();
            let text = get_text(&out.content);
            let entry_count = text.lines().filter(|l| l.contains("item_")).count();
            assert!(
                entry_count <= 5,
                "expected at most 5 entries, got {entry_count}"
            );
            let details = out.details.expect("expected limit details");
            assert_eq!(
                details
                    .get("entryLimitReached")
                    .and_then(serde_json::Value::as_u64),
                Some(5)
            );
        });
    }

    #[test]
    fn test_ls_zero_limit_is_rejected() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::write(tmp.path().join("item.txt"), "").unwrap();

            let tool = LsTool::new(tmp.path());
            let err = tool
                .execute(
                    "t",
                    serde_json::json!({
                        "path": tmp.path().to_string_lossy(),
                        "limit": 0
                    }),
                    None,
                )
                .await
                .expect_err("limit=0 should be rejected");

            assert!(
                err.to_string().contains("`limit` must be greater than 0"),
                "expected validation error, got: {err}"
            );
        });
    }

    #[test]
    fn test_ls_nonexistent_directory() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            let tool = LsTool::new(tmp.path());
            let err = tool
                .execute(
                    "t",
                    serde_json::json!({ "path": tmp.path().join("nope").to_string_lossy() }),
                    None,
                )
                .await;
            assert!(err.is_err());
        });
    }

    #[test]
    fn test_ls_empty_directory() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            let empty_dir = tmp.path().join("empty");
            std::fs::create_dir(&empty_dir).unwrap();

            let tool = LsTool::new(tmp.path());
            let out = tool
                .execute(
                    "t",
                    serde_json::json!({ "path": empty_dir.to_string_lossy() }),
                    None,
                )
                .await
                .unwrap();
            assert!(!out.is_error);
        });
    }

    #[test]
    fn test_ls_default_cwd() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::write(tmp.path().join("in_cwd.txt"), "").unwrap();

            let tool = LsTool::new(tmp.path());
            let out = tool
                .execute("t", serde_json::json!({}), None)
                .await
                .unwrap();
            let text = get_text(&out.content);
            assert!(
                text.contains("in_cwd.txt"),
                "expected cwd listing to include the file, got: {text}"
            );
        });
    }

    // ========================================================================
    // Additional helper tests
    // ========================================================================

    #[test]
    fn test_truncate_head_no_truncation() {
        let content = "short".to_string();
        let result = truncate_head(content, 100, 1000);
        assert!(!result.truncated);
        assert_eq!(result.content, "short");
        assert_eq!(result.truncated_by, None);
    }

    #[test]
    fn test_truncate_tail_no_truncation() {
        let content = "short".to_string();
        let result = truncate_tail(content, 100, 1000);
        assert!(!result.truncated);
        assert_eq!(result.content, "short");
    }

    #[test]
    fn test_truncate_head_empty_input() {
        let result = truncate_head(String::new(), 100, 1000);
        assert!(!result.truncated);
        assert_eq!(result.content, "");
    }

    #[test]
    fn test_truncate_tail_empty_input() {
        let result = truncate_tail(String::new(), 100, 1000);
        assert!(!result.truncated);
        assert_eq!(result.content, "");
    }

    #[test]
    fn test_detect_line_ending_crlf() {
        assert_eq!(detect_line_ending("hello\r\nworld"), "\r\n");
    }

    #[test]
    fn test_detect_line_ending_cr() {
        assert_eq!(detect_line_ending("hello\rworld"), "\r");
    }

    #[test]
    fn test_detect_line_ending_lf() {
        assert_eq!(detect_line_ending("hello\nworld"), "\n");
    }

    #[test]
    fn test_detect_line_ending_no_newline() {
        assert_eq!(detect_line_ending("hello world"), "\n");
    }

    #[test]
    fn test_normalize_to_lf() {
        assert_eq!(normalize_to_lf("a\r\nb\rc\nd"), "a\nb\nc\nd");
    }

    #[test]
    fn test_count_overlapping_occurrences() {
        assert_eq!(count_overlapping_occurrences("aaaa", "aa"), 3);
        assert_eq!(count_overlapping_occurrences("abababa", "aba"), 3);
        assert_eq!(count_overlapping_occurrences("abc", "d"), 0);
        assert_eq!(count_overlapping_occurrences("abc", ""), 0);
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 64, .. ProptestConfig::default() })]

        #[test]
        fn proptest_line_ending_roundtrip_invariant(
            input in arbitrary_text(),
            ending in prop_oneof![
                Just("\n".to_string()),
                Just("\r\n".to_string()),
                Just("\r".to_string()),
            ],
        ) {
            let normalized = normalize_to_lf(&input);
            let restored = restore_line_endings(&normalized, &ending);
            let renormalized = normalize_to_lf(&restored);
            prop_assert_eq!(renormalized, normalized);
        }
    }

    #[test]
    fn test_strip_bom_present() {
        let (result, had_bom) = strip_bom("\u{FEFF}hello");
        assert_eq!(result, "hello");
        assert!(had_bom);
    }

    #[test]
    fn test_strip_bom_absent() {
        let (result, had_bom) = strip_bom("hello");
        assert_eq!(result, "hello");
        assert!(!had_bom);
    }

    #[test]
    fn test_resolve_path_tilde_expansion() {
        let cwd = PathBuf::from("/home/user/project");
        let result = resolve_path("~/file.txt", &cwd);
        // Tilde expansion depends on environment, but should not be literal ~/
        assert!(!result.to_string_lossy().starts_with("~/"));
    }

    fn arbitrary_text() -> impl Strategy<Value = String> {
        prop::collection::vec(any::<u8>(), 0..512)
            .prop_map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    }

    fn match_char_strategy() -> impl Strategy<Value = char> {
        prop_oneof![
            8 => any::<char>(),
            1 => Just('\u{00A0}'),
            1 => Just('\u{202F}'),
            1 => Just('\u{205F}'),
            1 => Just('\u{3000}'),
            1 => Just('\u{2018}'),
            1 => Just('\u{2019}'),
            1 => Just('\u{201C}'),
            1 => Just('\u{201D}'),
            1 => Just('\u{201E}'),
            1 => Just('\u{201F}'),
            1 => Just('\u{2010}'),
            1 => Just('\u{2011}'),
            1 => Just('\u{2012}'),
            1 => Just('\u{2013}'),
            1 => Just('\u{2014}'),
            1 => Just('\u{2015}'),
            1 => Just('\u{2212}'),
            1 => Just('\u{200D}'),
            1 => Just('\u{0301}'),
        ]
    }

    fn arbitrary_match_text() -> impl Strategy<Value = String> {
        prop_oneof![
            9 => prop::collection::vec(match_char_strategy(), 0..2048),
            1 => prop::collection::vec(match_char_strategy(), 8192..16384),
        ]
        .prop_map(|chars| chars.into_iter().collect())
    }

    fn line_char_strategy() -> impl Strategy<Value = char> {
        prop_oneof![
            8 => any::<char>().prop_filter("single-line chars only", |c| *c != '\n'),
            1 => Just('é'),
            1 => Just('你'),
            1 => Just('😀'),
        ]
    }

    fn boundary_line_text() -> impl Strategy<Value = String> {
        prop_oneof![
            Just(0usize),
            Just(GREP_MAX_LINE_LENGTH.saturating_sub(1)),
            Just(GREP_MAX_LINE_LENGTH),
            Just(GREP_MAX_LINE_LENGTH + 1),
            0usize..(GREP_MAX_LINE_LENGTH + 128),
        ]
        .prop_flat_map(|len| {
            prop::collection::vec(line_char_strategy(), len)
                .prop_map(|chars| chars.into_iter().collect())
        })
    }

    fn safe_relative_segment() -> impl Strategy<Value = String> {
        prop_oneof![
            proptest::string::string_regex("[A-Za-z0-9._-]{1,12}")
                .expect("segment regex should compile"),
            Just("emoji😀".to_string()),
            Just("accent-é".to_string()),
            Just("rtl-עברית".to_string()),
            Just("line\nbreak".to_string()),
            Just("nul\0byte".to_string()),
        ]
        .prop_filter("segment cannot be . or ..", |segment| {
            segment != "." && segment != ".."
        })
    }

    fn safe_relative_path() -> impl Strategy<Value = String> {
        prop::collection::vec(safe_relative_segment(), 1..6).prop_map(|segments| segments.join("/"))
    }

    fn pathish_input() -> impl Strategy<Value = String> {
        prop_oneof![
            5 => safe_relative_path(),
            2 => safe_relative_path().prop_map(|p| format!("../{p}")),
            2 => safe_relative_path().prop_map(|p| format!("../../{p}")),
            1 => safe_relative_path().prop_map(|p| format!("/tmp/{p}")),
            1 => safe_relative_path().prop_map(|p| format!("~/{p}")),
            1 => Just("~".to_string()),
            1 => Just(".".to_string()),
            1 => Just("..".to_string()),
            1 => Just("././nested/../file.txt".to_string()),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 64, .. ProptestConfig::default() })]

        #[test]
        fn proptest_truncate_head_invariants(
            input in arbitrary_text(),
            max_lines in 0usize..32,
            max_bytes in 0usize..256,
        ) {
            let result = truncate_head(input.clone(), max_lines, max_bytes);

            prop_assert!(result.output_lines <= max_lines);
            prop_assert!(result.output_bytes <= max_bytes);
            prop_assert_eq!(result.output_bytes, result.content.len());

            prop_assert_eq!(result.truncated, result.truncated_by.is_some());
            prop_assert!(input.starts_with(&result.content));

            let repeat = truncate_head(result.content.clone(), max_lines, max_bytes);
            prop_assert_eq!(&repeat.content, &result.content);

            if result.truncated {
                prop_assert!(result.total_lines > max_lines || result.total_bytes > max_bytes);
            } else {
                prop_assert_eq!(&result.content, &input);
                prop_assert!(result.total_lines <= max_lines);
                prop_assert!(result.total_bytes <= max_bytes);
            }

            if result.first_line_exceeds_limit {
                prop_assert!(result.truncated);
                prop_assert_eq!(result.truncated_by, Some(TruncatedBy::Bytes));
                prop_assert!(result.output_bytes <= max_bytes);
                prop_assert!(result.output_lines <= 1);
                prop_assert!(input.starts_with(&result.content));
            }
        }

        #[test]
        fn proptest_truncate_tail_invariants(
            input in arbitrary_text(),
            max_lines in 0usize..32,
            max_bytes in 0usize..256,
        ) {
            let result = truncate_tail(input.clone(), max_lines, max_bytes);

            prop_assert!(result.output_lines <= max_lines);
            prop_assert!(result.output_bytes <= max_bytes);
            prop_assert_eq!(result.output_bytes, result.content.len());

            prop_assert_eq!(result.truncated, result.truncated_by.is_some());
            prop_assert!(input.ends_with(&result.content));

            let repeat = truncate_tail(result.content.clone(), max_lines, max_bytes);
            prop_assert_eq!(&repeat.content, &result.content);

            if result.last_line_partial {
                prop_assert!(result.truncated);
                prop_assert_eq!(result.truncated_by, Some(TruncatedBy::Bytes));
                // Partial output may span 1-2 lines when the input has a
                // trailing newline (the empty line after \n is preserved).
                prop_assert!(result.output_lines >= 1 && result.output_lines <= 2);
                let content_trimmed = result.content.trim_end_matches('\n');
                prop_assert!(input
                    .split('\n')
                    .rev()
                    .any(|line| line.ends_with(content_trimmed)));
            }
        }

        #[test]
        fn proptest_truncate_head_monotonic_limits(
            input in arbitrary_text(),
            max_lines_a in 0usize..32,
            max_lines_b in 0usize..32,
            max_bytes_a in 0usize..256,
            max_bytes_b in 0usize..256,
        ) {
            let low_lines = max_lines_a.min(max_lines_b);
            let high_lines = max_lines_a.max(max_lines_b);
            let low_bytes = max_bytes_a.min(max_bytes_b);
            let high_bytes = max_bytes_a.max(max_bytes_b);

            let small = truncate_head(input.clone(), low_lines, low_bytes);
            let large = truncate_head(input, high_lines, high_bytes);

            prop_assert!(large.content.starts_with(&small.content));
            prop_assert!(large.output_bytes >= small.output_bytes);
            prop_assert!(large.output_lines >= small.output_lines);
        }

        #[test]
        fn proptest_truncate_tail_monotonic_limits(
            input in arbitrary_text(),
            max_lines_a in 0usize..32,
            max_lines_b in 0usize..32,
            max_bytes_a in 0usize..256,
            max_bytes_b in 0usize..256,
        ) {
            let low_lines = max_lines_a.min(max_lines_b);
            let high_lines = max_lines_a.max(max_lines_b);
            let low_bytes = max_bytes_a.min(max_bytes_b);
            let high_bytes = max_bytes_a.max(max_bytes_b);

            let small = truncate_tail(input.clone(), low_lines, low_bytes);
            let large = truncate_tail(input, high_lines, high_bytes);

            prop_assert!(large.content.ends_with(&small.content));
            prop_assert!(large.output_bytes >= small.output_bytes);
            prop_assert!(large.output_lines >= small.output_lines);
        }

        #[test]
        fn proptest_truncate_head_prefix_invariant_under_append(
            base in arbitrary_text(),
            suffix in arbitrary_text(),
            max_lines in 0usize..32,
            max_bytes in 0usize..256,
        ) {
            let base_result = truncate_head(base.clone(), max_lines, max_bytes);
            let extended_result = truncate_head(format!("{base}{suffix}"), max_lines, max_bytes);
            prop_assert!(extended_result.content.starts_with(&base_result.content));
        }

        #[test]
        fn proptest_truncate_tail_suffix_invariant_under_prepend(
            base in arbitrary_text(),
            prefix in arbitrary_text(),
            max_lines in 0usize..32,
            max_bytes in 0usize..256,
        ) {
            let base_result = truncate_tail(base.clone(), max_lines, max_bytes);
            let extended_result = truncate_tail(format!("{prefix}{base}"), max_lines, max_bytes);
            prop_assert!(extended_result.content.ends_with(&base_result.content));
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 128, .. ProptestConfig::default() })]

        #[test]
        fn proptest_normalize_for_match_invariants(input in arbitrary_match_text()) {
            let normalized = normalize_for_match(&input);
            let renormalized = normalize_for_match(&normalized);

            prop_assert_eq!(&renormalized, &normalized);
            prop_assert!(normalized.len() <= input.len());
            prop_assert!(
                normalized.chars().all(|c| {
                    !is_special_unicode_space(c)
                        && !matches!(
                            c,
                            '\u{2018}'
                                | '\u{2019}'
                                | '\u{201C}'
                                | '\u{201D}'
                                | '\u{201E}'
                                | '\u{201F}'
                                | '\u{2010}'
                                | '\u{2011}'
                                | '\u{2012}'
                                | '\u{2013}'
                                | '\u{2014}'
                                | '\u{2015}'
                                | '\u{2212}'
                        )
                }),
                "normalize_for_match should remove target punctuation/space variants"
            );
        }

        #[test]
        fn proptest_truncate_line_boundary_invariants(line in boundary_line_text()) {
            const TRUNCATION_SUFFIX: &str = "... [truncated]";

            let result = truncate_line(&line, GREP_MAX_LINE_LENGTH);
            let line_char_count = line.chars().count();
            let suffix_chars = TRUNCATION_SUFFIX.chars().count();

            if line_char_count <= GREP_MAX_LINE_LENGTH {
                prop_assert!(!result.was_truncated);
                prop_assert_eq!(result.text, line);
            } else {
                prop_assert!(result.was_truncated);
                prop_assert!(result.text.ends_with(TRUNCATION_SUFFIX));
                let expected_prefix: String = line.chars().take(GREP_MAX_LINE_LENGTH).collect();
                let expected = format!("{expected_prefix}{TRUNCATION_SUFFIX}");
                prop_assert_eq!(&result.text, &expected);
                prop_assert!(result.text.chars().count() <= GREP_MAX_LINE_LENGTH + suffix_chars);
            }
        }

        #[test]
        fn proptest_resolve_path_safe_relative_invariants(relative_path in safe_relative_path()) {
            let cwd = PathBuf::from("/tmp/pi-agent-rust-tools-proptest");
            let resolved = resolve_path(&relative_path, &cwd);
            let normalized = normalize_dot_segments(&resolved);

            prop_assert_eq!(&resolved, &cwd.join(&relative_path));
            prop_assert!(resolved.starts_with(&cwd));
            prop_assert!(normalized.starts_with(&cwd));
            prop_assert_eq!(normalize_dot_segments(&normalized), normalized);
        }

        #[test]
        fn proptest_normalize_dot_segments_pathish_invariants(path_input in pathish_input()) {
            let cwd = PathBuf::from("/tmp/pi-agent-rust-tools-proptest");
            let resolved = resolve_path(&path_input, &cwd);
            let normalized_once = normalize_dot_segments(&resolved);
            let normalized_twice = normalize_dot_segments(&normalized_once);

            prop_assert_eq!(&normalized_once, &normalized_twice);
            prop_assert!(
                normalized_once
                    .components()
                    .all(|component| !matches!(component, std::path::Component::CurDir))
            );

            if std::path::Path::new(&path_input).is_absolute() {
                prop_assert!(resolved.is_absolute());
                prop_assert!(normalized_once.is_absolute());
            }
        }
    }

    // ========================================================================
    // Fuzzy find / edit-matching strategies
    // ========================================================================

    /// Strategy generating content text with occasional Unicode normalization
    /// targets (curly quotes, special spaces, em-dashes) and trailing
    /// whitespace.
    fn fuzzy_content_strategy() -> impl Strategy<Value = String> {
        prop::collection::vec(
            prop_oneof![
                8 => any::<char>().prop_filter("no nul", |c| *c != '\0'),
                1 => Just('\u{00A0}'),
                1 => Just('\u{2019}'),
                1 => Just('\u{201C}'),
                1 => Just('\u{2014}'),
            ],
            1..512,
        )
        .prop_map(|chars| chars.into_iter().collect())
    }

    /// Strategy for generating a needle substring from content. Picks a
    /// random sub-slice of the content (may be empty).
    fn needle_from_content(content: String) -> impl Strategy<Value = (String, String)> {
        let len = content.len();
        if len == 0 {
            return Just((content, String::new())).boxed();
        }
        (0..len)
            .prop_flat_map(move |start| {
                let c = content.clone();
                let remaining = c.len() - start;
                let max_needle = remaining.min(256);
                (Just(c), start..=start + max_needle.saturating_sub(1))
            })
            .prop_filter_map("valid char boundary", |(c, end)| {
                // Find the nearest valid char boundaries
                let start_candidates: Vec<usize> =
                    (0..c.len()).filter(|i| c.is_char_boundary(*i)).collect();
                if start_candidates.is_empty() {
                    return None;
                }
                let start = *start_candidates
                    .iter()
                    .min_by_key(|&&i| i.abs_diff(end.saturating_sub(end / 2)))
                    .unwrap_or(&0);
                let end_clamped = end.min(c.len());
                // Find next valid char boundary >= end_clamped
                let actual_end = (end_clamped..=c.len())
                    .find(|i| c.is_char_boundary(*i))
                    .unwrap_or(c.len());
                if start >= actual_end {
                    return Some((c, String::new()));
                }
                Some((c.clone(), c[start..actual_end].to_string()))
            })
            .boxed()
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 128, .. ProptestConfig::default() })]

        /// Exact substrings of content are always found by `fuzzy_find_text`.
        #[test]
        fn proptest_fuzzy_find_text_exact_match_invariants(
            (content, needle) in fuzzy_content_strategy().prop_flat_map(needle_from_content)
        ) {
            let result = fuzzy_find_text(&content, &needle);
            if needle.is_empty() {
                // Empty needle: exact match at index 0 (str::find("") == Some(0))
                prop_assert!(result.found, "empty needle should always match");
                prop_assert_eq!(result.index, 0);
                prop_assert_eq!(result.match_length, 0);
            } else {
                prop_assert!(
                    result.found,
                    "exact substring must be found: content len={}, needle len={}",
                    content.len(),
                    needle.len()
                );
                // The matched span should be valid UTF-8 byte indices
                prop_assert!(content.is_char_boundary(result.index));
                prop_assert!(content.is_char_boundary(result.index + result.match_length));
                // The matched text should contain the needle (exact match path)
                let matched = &content[result.index..result.index + result.match_length];
                prop_assert_eq!(matched, needle.as_str());
            }
        }

        /// Normalized text with Unicode variants is found via fuzzy matching.
        /// If we take content containing curly quotes / em-dashes, normalize
        /// it, then search for the normalized version, `fuzzy_find_text` must
        /// locate it.
        #[test]
        fn proptest_fuzzy_find_text_normalized_match_invariants(
            content in arbitrary_match_text()
        ) {
            // Normalize the whole content to get an ASCII-equivalent version
            let normalized = build_normalized_content(&content);
            if normalized.is_empty() {
                return Ok(());
            }
            // Take a prefix of normalized as needle (up to 128 chars)
            let needle_end = normalized
                .char_indices()
                .nth(128.min(normalized.chars().count().saturating_sub(1)))
                .map_or(normalized.len(), |(i, _)| i);
            // Find the nearest char boundary
            let needle_end = (needle_end..=normalized.len())
                .find(|i| normalized.is_char_boundary(*i))
                .unwrap_or(normalized.len());
            let needle = &normalized[..needle_end];
            if needle.is_empty() {
                return Ok(());
            }

            let result = fuzzy_find_text(&content, needle);
            prop_assert!(
                result.found,
                "normalized needle should be found via fuzzy match: needle={:?}",
                needle
            );
            // Verify the result points to valid UTF-8
            prop_assert!(content.is_char_boundary(result.index));
            prop_assert!(content.is_char_boundary(result.index + result.match_length));
        }

        /// `build_normalized_content` should be idempotent and never larger
        /// than the input.
        #[test]
        fn proptest_build_normalized_content_invariants(input in arbitrary_match_text()) {
            let normalized = build_normalized_content(&input);
            let renormalized = build_normalized_content(&normalized);

            // Idempotency
            prop_assert_eq!(
                &renormalized,
                &normalized,
                "build_normalized_content should be idempotent"
            );

            // Size: normalized text strips trailing whitespace per line and
            // may replace multi-byte Unicode with single-byte ASCII, so it
            // should never be larger than the input.
            prop_assert!(
                normalized.len() <= input.len(),
                "normalized should not be larger: {} vs {}",
                normalized.len(),
                input.len()
            );

            // Line count should be preserved (normalization does not add or
            // remove newlines).
            let input_lines = input.split('\n').count();
            let norm_lines = normalized.split('\n').count();
            prop_assert_eq!(
                norm_lines, input_lines,
                "line count must be preserved by normalization"
            );

            // No target Unicode chars should remain
            prop_assert!(
                normalized.chars().all(|c| {
                    !is_special_unicode_space(c)
                        && !matches!(
                            c,
                            '\u{2018}'
                                | '\u{2019}'
                                | '\u{201C}'
                                | '\u{201D}'
                                | '\u{201E}'
                                | '\u{201F}'
                                | '\u{2010}'
                                | '\u{2011}'
                                | '\u{2012}'
                                | '\u{2013}'
                                | '\u{2014}'
                                | '\u{2015}'
                                | '\u{2212}'
                        )
                }),
                "normalized content should not contain target Unicode chars"
            );
        }

        /// Appending trailing whitespace to each line should not change the
        /// normalized content (metamorphic invariant).
        #[test]
        fn proptest_build_normalized_content_trailing_whitespace_invariant(
            input in arbitrary_match_text()
        ) {
            let normalized = build_normalized_content(&input);
            let mut with_trailing = String::new();
            let mut lines = input.split('\n').peekable();

            while let Some(line) = lines.next() {
                with_trailing.push_str(line);
                with_trailing.push_str("  \t");
                if lines.peek().is_some() {
                    with_trailing.push('\n');
                }
            }

            let normalized_trailing = build_normalized_content(&with_trailing);
            prop_assert_eq!(normalized_trailing, normalized);
        }

        /// `map_normalized_range_to_original` should produce valid byte
        /// ranges in the original content and the extracted original slice,
        /// when re-normalized, should start with the expected normalized
        /// prefix. Trailing whitespace at line ends makes an exact match
        /// impossible (normalization strips it), so we verify the key
        /// structural invariant: the range is valid and the non-whitespace
        /// content round-trips correctly.
        #[test]
        fn proptest_map_normalized_range_roundtrip(input in arbitrary_match_text()) {
            let normalized = build_normalized_content(&input);
            if normalized.is_empty() {
                return Ok(());
            }

            // Pick a range in the normalized text at char boundaries
            let norm_chars: Vec<(usize, char)> = normalized.char_indices().collect();
            let norm_len = norm_chars.len();
            if norm_len == 0 {
                return Ok(());
            }

            // Use the first quarter as the match range for determinism
            let end_char = (norm_len / 4).max(1).min(norm_len);
            let norm_start = norm_chars[0].0;
            let norm_end = if end_char < norm_chars.len() {
                norm_chars[end_char].0
            } else {
                normalized.len()
            };
            let norm_match_len = norm_end - norm_start;

            let (orig_start, orig_len) =
                map_normalized_range_to_original(&input, norm_start, norm_match_len);

            // Invariant 1: result is within input bounds
            prop_assert!(
                orig_start + orig_len <= input.len(),
                "mapped range {orig_start}..{} exceeds input len {}",
                orig_start + orig_len,
                input.len()
            );

            // Invariant 2: result is at valid char boundaries
            prop_assert!(
                input.is_char_boundary(orig_start),
                "orig_start {} is not a char boundary",
                orig_start
            );
            prop_assert!(
                input.is_char_boundary(orig_start + orig_len),
                "orig_end {} is not a char boundary",
                orig_start + orig_len
            );

            // Invariant 3: original range is at least as large as
            // normalized range (original may include trailing whitespace
            // and multi-byte Unicode chars that normalize to fewer bytes)
            prop_assert!(
                orig_len >= norm_match_len
                    || orig_len == 0
                    || norm_match_len == 0,
                "original range ({orig_len}) should be >= normalized range ({norm_match_len})"
            );

            // Invariant 4: the normalized expected slice, when searched
            // for in the original content via fuzzy_find_text, should be
            // found at or before the mapped position.
            let expected_norm = &normalized[norm_start..norm_end];
            if !expected_norm.is_empty() {
                let fuzzy_result = fuzzy_find_text(&input, expected_norm);
                prop_assert!(
                    fuzzy_result.found,
                    "normalized needle should be findable in original content"
                );
            }
        }
    }

    #[test]
    fn test_truncate_head_preserves_newline() {
        // "Line1\nLine2" truncated to 1 line should be "Line1\n"
        let content = "Line1\nLine2".to_string();
        let result = truncate_head(content, 1, 1000);
        assert_eq!(result.content, "Line1\n");

        // "Line1" truncated to 1 line should be "Line1"
        let content = "Line1".to_string();
        let result = truncate_head(content, 1, 1000);
        assert_eq!(result.content, "Line1");

        // "Line1\n" truncated to 1 line should be "Line1\n"
        let content = "Line1\n".to_string();
        let result = truncate_head(content, 1, 1000);
        assert_eq!(result.content, "Line1\n");
    }

    #[test]
    fn test_edit_crlf_content_correctness() {
        // Regression test: ensure we don't mix original indices with normalized content slices.
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("crlf.txt");
            // "line1" (5) + "\r\n" (2) + "line2" (5) + "\r\n" (2) + "line3" (5) = 19 bytes
            let content = "line1\r\nline2\r\nline3";
            std::fs::write(&path, content).unwrap();

            let tool = EditTool::new(tmp.path());

            // Replacing "line2" should work correctly and preserve CRLF.
            // Original "line2" is at index 7. Normalized "line2" is at index 6.
            // If we used original index (7) on normalized string ("line1\nline2\nline3"),
            // we would start at "ine2..." instead of "line2...", corrupting the file.
            let out = tool
                .execute(
                    "t",
                    serde_json::json!({
                        "path": path.to_string_lossy(),
                        "oldText": "line2",
                        "newText": "changed"
                    }),
                    None,
                )
                .await
                .unwrap();

            assert!(!out.is_error);
            let new_content = std::fs::read_to_string(&path).unwrap();

            // Expect: "line1\r\nchanged\r\nline3"
            assert_eq!(new_content, "line1\r\nchanged\r\nline3");
        });
    }

    #[test]
    fn test_edit_cr_content_correctness() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("cr.txt");
            std::fs::write(&path, "line1\rline2\rline3").unwrap();

            let tool = EditTool::new(tmp.path());
            let out = tool
                .execute(
                    "t",
                    serde_json::json!({
                        "path": path.to_string_lossy(),
                        "oldText": "line2",
                        "newText": "changed"
                    }),
                    None,
                )
                .await
                .unwrap();

            assert!(!out.is_error);
            let new_content = std::fs::read_to_string(&path).unwrap();
            assert_eq!(new_content, "line1\rchanged\rline3");
        });
    }

    // ========================================================================
    // Hashline tests
    // ========================================================================

    #[test]
    fn test_compute_line_hash_basic() {
        // Same content at same index should produce same hash
        let h1 = compute_line_hash(0, "fn main() {");
        let h2 = compute_line_hash(0, "fn main() {");
        assert_eq!(h1, h2);

        // Different content should (usually) produce different hash
        let h3 = compute_line_hash(0, "fn foo() {");
        // Not guaranteed different for all inputs, but these specific ones should differ
        assert_ne!(h1, h3);

        // Hash is 2 bytes from NIBBLE_STR
        for &b in &h1 {
            assert!(NIBBLE_STR.contains(&b), "hash byte {b} not in NIBBLE_STR");
        }
    }

    #[test]
    fn test_compute_line_hash_punctuation_only() {
        // Punctuation-only lines use line_idx as seed, so same content at
        // different indices should produce different hashes.
        let h1 = compute_line_hash(0, "}");
        let h2 = compute_line_hash(1, "}");
        assert_ne!(
            h1, h2,
            "punctuation-only lines at different indices should differ"
        );

        // Blank lines also use idx as seed
        let h3 = compute_line_hash(0, "");
        let h4 = compute_line_hash(1, "");
        assert_ne!(h3, h4);
    }

    #[test]
    fn test_compute_line_hash_whitespace_invariant() {
        // Leading/trailing whitespace should not affect hash (whitespace stripped)
        let h1 = compute_line_hash(0, "return 42;");
        let h2 = compute_line_hash(0, "    return 42;");
        let h3 = compute_line_hash(0, "\treturn 42;");
        assert_eq!(h1, h2);
        assert_eq!(h1, h3);
    }

    #[test]
    fn test_format_hashline_tag() {
        let tag = format_hashline_tag(0, "fn main() {");
        // Should be "1#XX" format (1-indexed)
        assert!(
            tag.starts_with("1#"),
            "tag should start with 1#, got: {tag}"
        );
        assert_eq!(tag.len(), 4, "tag should be 4 chars: N#AB");

        let tag10 = format_hashline_tag(9, "line 10");
        assert!(tag10.starts_with("10#"));
        assert_eq!(tag10.len(), 5); // "10#AB"
    }

    #[test]
    fn test_parse_hashline_tag_valid() {
        // Simple valid tag
        let (line, hash) = parse_hashline_tag("5#KJ").unwrap();
        assert_eq!(line, 5);
        assert_eq!(hash, [b'K', b'J']);

        // With spaces around #
        let (line, hash) = parse_hashline_tag("  10 # QR ").unwrap();
        assert_eq!(line, 10);
        assert_eq!(hash, [b'Q', b'R']);

        // With diff markers
        let (line, hash) = parse_hashline_tag("> + 3#ZZ").unwrap();
        assert_eq!(line, 3);
        assert_eq!(hash, [b'Z', b'Z']);
    }

    #[test]
    fn test_parse_hashline_tag_invalid() {
        // Line number 0
        assert!(parse_hashline_tag("0#KJ").is_err());
        // No hash
        assert!(parse_hashline_tag("5#").is_err());
        // Invalid chars in hash
        assert!(parse_hashline_tag("5#AA").is_err()); // 'A' not in NIBBLE_STR
        // No number
        assert!(parse_hashline_tag("#KJ").is_err());
        // Empty
        assert!(parse_hashline_tag("").is_err());
    }

    #[test]
    fn test_strip_hashline_prefix() {
        assert_eq!(strip_hashline_prefix("5#KJ:hello world"), "hello world");
        assert_eq!(strip_hashline_prefix("100#ZZ:fn main() {"), "fn main() {");
        assert_eq!(strip_hashline_prefix(" 5 # KJ:hello world"), "hello world");
        assert_eq!(strip_hashline_prefix("> + 5#KJ:hello world"), "hello world");
        assert_eq!(strip_hashline_prefix("5#KJ :hello world"), "hello world");
        // No prefix → unchanged
        assert_eq!(strip_hashline_prefix("hello world"), "hello world");
        assert_eq!(strip_hashline_prefix(""), "");
    }

    #[test]
    fn test_hashline_edit_single_replace() {
        asupersync::test_utils::run_test(|| async {
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("test.txt");
            std::fs::write(&file, "line1\nline2\nline3\n").unwrap();

            let tool = HashlineEditTool::new(dir.path());

            // Get the hash for line 2 (idx=1)
            let tag2 = format_hashline_tag(1, "line2");

            let input = serde_json::json!({
                "path": file.to_str().unwrap(),
                "edits": [{
                    "op": "replace",
                    "pos": tag2,
                    "lines": ["changed"]
                }]
            });

            let out = tool.execute("test", input, None).await.unwrap();
            assert!(!out.is_error);

            let content = std::fs::read_to_string(&file).unwrap();
            assert_eq!(content, "line1\nchanged\nline3\n");
        });
    }

    #[test]
    fn test_hashline_edit_range_replace() {
        asupersync::test_utils::run_test(|| async {
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("test.txt");
            std::fs::write(&file, "a\nb\nc\nd\ne\n").unwrap();

            let tool = HashlineEditTool::new(dir.path());

            let tag_b = format_hashline_tag(1, "b");
            let tag_d = format_hashline_tag(3, "d");

            let input = serde_json::json!({
                "path": file.to_str().unwrap(),
                "edits": [{
                    "op": "replace",
                    "pos": tag_b,
                    "end": tag_d,
                    "lines": ["X", "Y"]
                }]
            });

            let out = tool.execute("test", input, None).await.unwrap();
            assert!(!out.is_error);

            let content = std::fs::read_to_string(&file).unwrap();
            assert_eq!(content, "a\nX\nY\ne\n");
        });
    }

    #[test]
    fn test_hashline_edit_prepend() {
        asupersync::test_utils::run_test(|| async {
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("test.txt");
            std::fs::write(&file, "a\nb\nc\n").unwrap();

            let tool = HashlineEditTool::new(dir.path());
            let tag_b = format_hashline_tag(1, "b");

            let input = serde_json::json!({
                "path": file.to_str().unwrap(),
                "edits": [{
                    "op": "prepend",
                    "pos": tag_b,
                    "lines": ["inserted"]
                }]
            });

            let out = tool.execute("test", input, None).await.unwrap();
            assert!(!out.is_error);

            let content = std::fs::read_to_string(&file).unwrap();
            assert_eq!(content, "a\ninserted\nb\nc\n");
        });
    }

    #[test]
    fn test_hashline_edit_append() {
        asupersync::test_utils::run_test(|| async {
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("test.txt");
            std::fs::write(&file, "a\nb\nc\n").unwrap();

            let tool = HashlineEditTool::new(dir.path());
            let tag_b = format_hashline_tag(1, "b");

            let input = serde_json::json!({
                "path": file.to_str().unwrap(),
                "edits": [{
                    "op": "append",
                    "pos": tag_b,
                    "lines": ["inserted"]
                }]
            });

            let out = tool.execute("test", input, None).await.unwrap();
            assert!(!out.is_error);

            let content = std::fs::read_to_string(&file).unwrap();
            assert_eq!(content, "a\nb\ninserted\nc\n");
        });
    }

    #[test]
    fn test_hashline_edit_bottom_up_ordering() {
        asupersync::test_utils::run_test(|| async {
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("test.txt");
            std::fs::write(&file, "a\nb\nc\nd\n").unwrap();

            let tool = HashlineEditTool::new(dir.path());
            let tag_b = format_hashline_tag(1, "b");
            let tag_d = format_hashline_tag(3, "d");

            // Two edits at different positions — both should apply correctly
            let input = serde_json::json!({
                "path": file.to_str().unwrap(),
                "edits": [
                    { "op": "replace", "pos": tag_b, "lines": ["B"] },
                    { "op": "replace", "pos": tag_d, "lines": ["D"] }
                ]
            });

            let out = tool.execute("test", input, None).await.unwrap();
            assert!(!out.is_error);

            let content = std::fs::read_to_string(&file).unwrap();
            assert_eq!(content, "a\nB\nc\nD\n");
        });
    }

    #[test]
    fn test_hashline_edit_hash_mismatch() {
        asupersync::test_utils::run_test(|| async {
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("test.txt");
            std::fs::write(&file, "hello\nworld\n").unwrap();

            let tool = HashlineEditTool::new(dir.path());

            // Use a deliberately wrong hash
            let input = serde_json::json!({
                "path": file.to_str().unwrap(),
                "edits": [{
                    "op": "replace",
                    "pos": "1#ZZ",
                    "lines": ["changed"]
                }]
            });

            let result = tool.execute("test", input, None).await;
            assert!(result.is_err());
            let err_msg = result.unwrap_err().to_string();
            assert!(
                err_msg.contains("Hash validation failed"),
                "error should mention hash validation: {err_msg}"
            );
        });
    }

    #[test]
    fn test_hashline_edit_dedup() {
        asupersync::test_utils::run_test(|| async {
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("test.txt");
            std::fs::write(&file, "a\nb\nc\n").unwrap();

            let tool = HashlineEditTool::new(dir.path());
            let tag_b = format_hashline_tag(1, "b");

            // Duplicate edits should be deduplicated
            let input = serde_json::json!({
                "path": file.to_str().unwrap(),
                "edits": [
                    { "op": "replace", "pos": &tag_b, "lines": ["B"] },
                    { "op": "replace", "pos": &tag_b, "lines": ["B"] }
                ]
            });

            let out = tool.execute("test", input, None).await.unwrap();
            assert!(!out.is_error);

            let content = std::fs::read_to_string(&file).unwrap();
            assert_eq!(content, "a\nB\nc\n");
        });
    }

    #[test]
    fn test_hashline_edit_noop_detection() {
        asupersync::test_utils::run_test(|| async {
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("test.txt");
            std::fs::write(&file, "a\nb\nc\n").unwrap();

            let tool = HashlineEditTool::new(dir.path());
            let tag_b = format_hashline_tag(1, "b");

            // Replacing with identical content is a no-op
            let input = serde_json::json!({
                "path": file.to_str().unwrap(),
                "edits": [{
                    "op": "replace",
                    "pos": &tag_b,
                    "lines": ["b"]
                }]
            });

            let result = tool.execute("test", input, None).await;
            assert!(result.is_err());
            let err_msg = result.unwrap_err().to_string();
            assert!(
                err_msg.contains("no-ops"),
                "error should mention no-ops: {err_msg}"
            );
        });
    }

    #[test]
    fn test_hashline_read_output_format() {
        asupersync::test_utils::run_test(|| async {
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("test.txt");
            std::fs::write(&file, "fn main() {\n    println!(\"hello\");\n}\n").unwrap();

            let tool = ReadTool::new(dir.path());
            let input = serde_json::json!({
                "path": file.to_str().unwrap(),
                "hashline": true
            });

            let out = tool.execute("test", input, None).await.unwrap();
            assert!(!out.is_error);
            let text = get_text(&out.content);

            // Each line should be in N#AB:content format
            for line in text.lines() {
                if line.starts_with('[') || line.is_empty() {
                    continue; // skip metadata lines
                }
                assert!(
                    hashline_tag_regex().is_match(line),
                    "line should match hashline format: {line:?}"
                );
                assert!(
                    line.contains(':'),
                    "line should contain ':' separator: {line:?}"
                );
            }

            // First line should start with "1#"
            let first_line = text.lines().next().unwrap();
            assert!(first_line.starts_with("1#"), "first line: {first_line:?}");
        });
    }

    #[test]
    fn test_hashline_edit_prefix_stripping() {
        asupersync::test_utils::run_test(|| async {
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("test.txt");
            std::fs::write(&file, "a\nb\nc\n").unwrap();

            let tool = HashlineEditTool::new(dir.path());
            let tag_b = format_hashline_tag(1, "b");

            // Model copies hashline tags into replacement — they should be stripped
            let input = serde_json::json!({
                "path": file.to_str().unwrap(),
                "edits": [{
                    "op": "replace",
                    "pos": &tag_b,
                    "lines": ["2#KJ:changed"]
                }]
            });

            let out = tool.execute("test", input, None).await.unwrap();
            assert!(!out.is_error);

            let content = std::fs::read_to_string(&file).unwrap();
            assert_eq!(content, "a\nchanged\nc\n");
        });
    }

    #[test]
    fn test_hashline_edit_delete_lines() {
        asupersync::test_utils::run_test(|| async {
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("test.txt");
            std::fs::write(&file, "a\nb\nc\nd\n").unwrap();

            let tool = HashlineEditTool::new(dir.path());
            let tag_b = format_hashline_tag(1, "b");
            let tag_c = format_hashline_tag(2, "c");

            // Replace range with null (delete)
            let input = serde_json::json!({
                "path": file.to_str().unwrap(),
                "edits": [{
                    "op": "replace",
                    "pos": &tag_b,
                    "end": &tag_c,
                    "lines": null
                }]
            });

            let out = tool.execute("test", input, None).await.unwrap();
            assert!(!out.is_error);

            let content = std::fs::read_to_string(&file).unwrap();
            assert_eq!(content, "a\nd\n");
        });
    }

    #[test]
    fn test_hashline_edit_crlf_preservation() {
        asupersync::test_utils::run_test(|| async {
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("test.txt");
            std::fs::write(&file, "line1\r\nline2\r\nline3").unwrap();

            let tool = HashlineEditTool::new(dir.path());
            let tag2 = format_hashline_tag(1, "line2");

            let input = serde_json::json!({
                "path": file.to_str().unwrap(),
                "edits": [{
                    "op": "replace",
                    "pos": tag2,
                    "lines": ["changed"]
                }]
            });

            let out = tool.execute("test", input, None).await.unwrap();
            assert!(!out.is_error);

            let content = std::fs::read_to_string(&file).unwrap();
            assert_eq!(content, "line1\r\nchanged\r\nline3");
        });
    }

    #[test]
    fn test_hashline_edit_cr_preservation() {
        asupersync::test_utils::run_test(|| async {
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("test.txt");
            std::fs::write(&file, "line1\rline2\rline3").unwrap();

            let tool = HashlineEditTool::new(dir.path());
            let tag2 = format_hashline_tag(1, "line2");

            let input = serde_json::json!({
                "path": file.to_str().unwrap(),
                "edits": [{
                    "op": "replace",
                    "pos": tag2,
                    "lines": ["changed"]
                }]
            });

            let out = tool.execute("test", input, None).await.unwrap();
            assert!(!out.is_error);

            let content = std::fs::read_to_string(&file).unwrap();
            assert_eq!(content, "line1\rchanged\rline3");
        });
    }

    #[test]
    fn test_hashline_edit_empty_file_append() {
        asupersync::test_utils::run_test(|| async {
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("empty.txt");
            std::fs::write(&file, "").unwrap();

            let tool = HashlineEditTool::new(dir.path());

            // EOF append with no pos on empty file
            let input = serde_json::json!({
                "path": file.to_str().unwrap(),
                "edits": [{
                    "op": "append",
                    "lines": ["new_line"]
                }]
            });

            let out = tool.execute("test", input, None).await.unwrap();
            assert!(!out.is_error);

            let content = std::fs::read_to_string(&file).unwrap();
            assert!(content.contains("new_line"));
        });
    }

    #[test]
    fn test_hashline_edit_single_line_no_trailing_newline() {
        asupersync::test_utils::run_test(|| async {
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("single.txt");
            std::fs::write(&file, "hello").unwrap();

            let tool = HashlineEditTool::new(dir.path());
            let tag = format_hashline_tag(0, "hello");

            let input = serde_json::json!({
                "path": file.to_str().unwrap(),
                "edits": [{
                    "op": "replace",
                    "pos": tag,
                    "lines": ["world"]
                }]
            });

            let out = tool.execute("test", input, None).await.unwrap();
            assert!(!out.is_error);

            let content = std::fs::read_to_string(&file).unwrap();
            assert_eq!(content, "world");
        });
    }

    #[test]
    fn test_hashline_edit_preserves_bom_hash_validation() {
        asupersync::test_utils::run_test(|| async {
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("bom.txt");
            let bom = "\u{FEFF}";
            std::fs::write(&file, format!("{bom}alpha\nbeta\n")).unwrap();

            let tool = HashlineEditTool::new(dir.path());
            let tag1 = format_hashline_tag(0, &format!("{bom}alpha"));

            let input = serde_json::json!({
                "path": file.to_str().unwrap(),
                "edits": [{
                    "op": "replace",
                    "pos": tag1,
                    "lines": ["gamma"]
                }]
            });

            let out = tool.execute("test", input, None).await.unwrap();
            assert!(!out.is_error);

            let content = std::fs::read_to_string(&file).unwrap();
            assert_eq!(content, format!("{bom}gamma\nbeta\n"));
        });
    }

    #[test]
    fn test_hashline_edit_bof_prepend_no_pos() {
        asupersync::test_utils::run_test(|| async {
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("test.txt");
            std::fs::write(&file, "a\nb\nc\n").unwrap();

            let tool = HashlineEditTool::new(dir.path());

            // Prepend with no pos should insert at BOF (before line 0)
            let input = serde_json::json!({
                "path": file.to_str().unwrap(),
                "edits": [{
                    "op": "prepend",
                    "lines": ["header"]
                }]
            });

            let out = tool.execute("test", input, None).await.unwrap();
            assert!(!out.is_error);

            let content = std::fs::read_to_string(&file).unwrap();
            assert_eq!(content, "header\na\nb\nc\n");
        });
    }

    #[test]
    fn test_hashline_edit_eof_append_no_pos() {
        asupersync::test_utils::run_test(|| async {
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("test.txt");
            std::fs::write(&file, "a\nb\nc\n").unwrap();

            let tool = HashlineEditTool::new(dir.path());

            // Append with no pos should insert at EOF (after last line)
            let input = serde_json::json!({
                "path": file.to_str().unwrap(),
                "edits": [{
                    "op": "append",
                    "lines": ["footer"]
                }]
            });

            let out = tool.execute("test", input, None).await.unwrap();
            assert!(!out.is_error);

            let content = std::fs::read_to_string(&file).unwrap();
            assert!(
                content.contains("footer"),
                "content should contain footer: {content:?}"
            );
        });
    }

    #[test]
    fn test_hashline_edit_overlapping_replace_ranges_rejected() {
        asupersync::test_utils::run_test(|| async {
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("test.txt");
            std::fs::write(&file, "a\nb\nc\nd\ne\n").unwrap();

            let tool = HashlineEditTool::new(dir.path());
            let tag_b = format_hashline_tag(1, "b");
            let tag_d = format_hashline_tag(3, "d");
            let tag_c = format_hashline_tag(2, "c");
            let tag_e = format_hashline_tag(4, "e");

            // Two overlapping replace ranges: lines 2-4 and lines 3-5
            let input = serde_json::json!({
                "path": file.to_str().unwrap(),
                "edits": [
                    { "op": "replace", "pos": &tag_b, "end": &tag_d, "lines": ["X"] },
                    { "op": "replace", "pos": &tag_c, "end": &tag_e, "lines": ["Y"] }
                ]
            });

            let result = tool.execute("test", input, None).await;
            assert!(result.is_err());
            let err_msg = result.unwrap_err().to_string();
            assert!(
                err_msg.contains("Overlapping"),
                "error should mention overlapping: {err_msg}"
            );
        });
    }

    #[test]
    fn test_hashline_edit_reversed_range_rejected() {
        asupersync::test_utils::run_test(|| async {
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("test.txt");
            std::fs::write(&file, "a\nb\nc\nd\n").unwrap();

            let tool = HashlineEditTool::new(dir.path());
            let tag_b = format_hashline_tag(1, "b");
            let tag_d = format_hashline_tag(3, "d");

            // End anchor before start anchor
            let input = serde_json::json!({
                "path": file.to_str().unwrap(),
                "edits": [{
                    "op": "replace",
                    "pos": &tag_d,
                    "end": &tag_b,
                    "lines": ["X"]
                }]
            });

            let result = tool.execute("test", input, None).await;
            assert!(result.is_err());
            let err_msg = result.unwrap_err().to_string();
            assert!(
                err_msg.contains("before start"),
                "error should mention before start: {err_msg}"
            );
        });
    }

    #[test]
    fn test_hashline_edit_trailing_newline_semantics() {
        asupersync::test_utils::run_test(|| async {
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("test.txt");
            // File with trailing newline: split produces ["line1", "line2", ""]
            std::fs::write(&file, "line1\nline2\n").unwrap();

            let tool = HashlineEditTool::new(dir.path());
            let tag2 = format_hashline_tag(1, "line2");

            // Replace line2, trailing newline should be preserved
            let input = serde_json::json!({
                "path": file.to_str().unwrap(),
                "edits": [{
                    "op": "replace",
                    "pos": tag2,
                    "lines": ["changed"]
                }]
            });

            let out = tool.execute("test", input, None).await.unwrap();
            assert!(!out.is_error);

            let content = std::fs::read_to_string(&file).unwrap();
            assert_eq!(content, "line1\nchanged\n");
        });
    }
}
