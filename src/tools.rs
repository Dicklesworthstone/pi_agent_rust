//! Built-in tool implementations.
//!
//! Pi provides built-in file, shell, search, editing, and subagent delegation tools.
//!
//! Tools are exposed to the model via JSON Schema (see [`crate::provider::ToolDef`]) and executed
//! locally by the agent loop. Each tool returns structured [`ContentBlock`] output suitable for
//! rendering in the TUI and for inclusion in provider messages as tool results.

use crate::agent_cx::AgentCx;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::extensions::{safe_canonicalize, strip_unc_prefix};
use crate::model::{ContentBlock, ImageContent, TextContent};
use crate::platform::{
    EffectiveModeAccessContext, UNIX_ACCESS_READ, UNIX_ACCESS_SEARCH, UNIX_ACCESS_WRITE,
    ensure_effective_mode_access,
};
use crate::workspace::{WorkspaceHandle, ensure_canonical_path_allowed};
use asupersync::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, ReadBuf, SeekFrom};
use asupersync::time::{sleep, wall_now};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::Digest as _;
use std::cmp::Ordering;
use std::collections::{HashMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

// ============================================================================
// Tool Trait
// ============================================================================

/// Coarse side-effect declaration for tool scheduling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolEffects {
    bits: u8,
}

impl ToolEffects {
    const READ: u8 = 1 << 0;
    const WRITE: u8 = 1 << 1;
    const APPEND: u8 = 1 << 2;
    const NETWORK: u8 = 1 << 3;
    const PROCESS: u8 = 1 << 4;
    const BARRIER: u8 = Self::WRITE | Self::APPEND | Self::PROCESS;

    /// Tool reads local state without mutating it.
    #[must_use]
    pub const fn read() -> Self {
        Self { bits: Self::READ }
    }

    /// Tool may create, replace, or otherwise mutate local state.
    #[must_use]
    pub const fn write() -> Self {
        Self { bits: Self::WRITE }
    }

    /// Tool appends to existing local state.
    #[must_use]
    pub const fn append() -> Self {
        Self { bits: Self::APPEND }
    }

    /// Tool performs network I/O but does not mutate local state.
    #[must_use]
    pub const fn network() -> Self {
        Self {
            bits: Self::NETWORK,
        }
    }

    /// Tool starts a local process. This is treated as a scheduling barrier.
    #[must_use]
    pub const fn process() -> Self {
        Self {
            bits: Self::PROCESS,
        }
    }

    /// Combine multiple effect declarations for a single tool or batch.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self {
            bits: self.bits | other.bits,
        }
    }

    /// Whether this declaration reads local state.
    #[must_use]
    pub const fn reads(self) -> bool {
        self.bits & Self::READ != 0
    }

    /// Whether this declaration may mutate local state by replacing content.
    #[must_use]
    pub const fn writes(self) -> bool {
        self.bits & Self::WRITE != 0
    }

    /// Whether this declaration may append to local state.
    #[must_use]
    pub const fn appends(self) -> bool {
        self.bits & Self::APPEND != 0
    }

    /// Whether this declaration performs network I/O.
    #[must_use]
    pub const fn networks(self) -> bool {
        self.bits & Self::NETWORK != 0
    }

    /// Whether this declaration starts or controls a local process.
    #[must_use]
    pub const fn processes(self) -> bool {
        self.bits & Self::PROCESS != 0
    }

    /// Stable labels for machine-readable scheduling evidence.
    #[must_use]
    pub fn labels(self) -> Vec<&'static str> {
        let mut labels = Vec::with_capacity(5);
        if self.reads() {
            labels.push("read");
        }
        if self.writes() {
            labels.push("write");
        }
        if self.appends() {
            labels.push("append");
        }
        if self.networks() {
            labels.push("network");
        }
        if self.processes() {
            labels.push("process");
        }
        labels
    }

    /// Whether this effect set can run in a compatible concurrent batch.
    #[must_use]
    pub const fn parallel_safe(self) -> bool {
        self.bits != 0 && self.bits & Self::BARRIER == 0
    }

    /// Whether two effect sets can share a concurrent batch.
    #[must_use]
    pub const fn compatible_with(self, other: Self) -> bool {
        self.parallel_safe() && other.parallel_safe()
    }
}

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

    /// Declare the coarse side effects used by the agent scheduler.
    ///
    /// Defaults to local write effects so undeclared tools are serialized fail-closed.
    #[must_use]
    fn effects(&self) -> ToolEffects {
        ToolEffects::write()
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

/// Hard cap for complete find scans before newest-first selection.
pub const FIND_SCAN_HARD_LIMIT: usize = 20_000;

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
const BASH_CANCELLATION_SCHEMA_V1: &str = "pi.tool.bash.cancellation.v1";

/// Hard limit for bash output file size (1GB) to prevent disk exhaustion DoS.
pub(crate) const BASH_FILE_LIMIT_BYTES: usize = 1024 * 1024 * 1024; // 1 GiB

const TOOL_OUTPUT_ARTIFACT_SCHEMA_V1: &str = "pi.tool_output_artifact.v1";
const TOOL_OUTPUT_ARTIFACT_REDACTION_POLICY_V1: &str = "pi.tool_output_artifact.redaction.v1";
const TOOL_OUTPUT_ARTIFACT_RETENTION_CLASS: &str = "session_scoped_temp_evidence";
const TOOL_OUTPUT_ARTIFACT_SPILLOVER_REASON: &str = "sourceBytesExceededPreviewThreshold";
const TOOL_OUTPUT_ARTIFACT_THRESHOLD_BYTES: usize = DEFAULT_MAX_BYTES;
const TOOL_OUTPUT_ARTIFACT_REDACTION_MAX_BYTES_USIZE: usize = 64 * 1024 * 1024;
const TOOL_OUTPUT_ARTIFACT_REDACTION_MAX_BYTES: u64 = 64 * 1024 * 1024;
const TOOL_OUTPUT_ARTIFACT_MAX_BYTES_USIZE: usize = 1024 * 1024 * 1024;
const TOOL_OUTPUT_ARTIFACT_MAX_BYTES: u64 = 1024 * 1024 * 1024;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BashCancellationReason {
    Timeout,
    AmbientCancellation,
}

impl BashCancellationReason {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::AmbientCancellation => "ambient_cancellation",
        }
    }
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
        content.clear();
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
        content.clear();
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

struct HeadTruncatingLineWriter {
    content: String,
    max_bytes: usize,
    total_lines: usize,
    total_bytes: usize,
    output_lines: usize,
    truncated: bool,
    last_line_partial: bool,
    first_line_exceeds_limit: bool,
}

impl HeadTruncatingLineWriter {
    fn new(max_bytes: usize) -> Self {
        Self {
            content: String::with_capacity(max_bytes.min(8192)),
            max_bytes,
            total_lines: 0,
            total_bytes: 0,
            output_lines: 0,
            truncated: false,
            last_line_partial: false,
            first_line_exceeds_limit: false,
        }
    }

    fn push_line(&mut self, line: &str) {
        debug_assert!(!line.contains('\n'));

        let line_index = self.total_lines;
        let separator_len = usize::from(line_index > 0);
        let piece_bytes = separator_len.saturating_add(line.len());
        self.total_lines = self.total_lines.saturating_add(1);
        self.total_bytes = self.total_bytes.saturating_add(piece_bytes);

        if self.truncated {
            return;
        }

        if self.max_bytes == 0 {
            self.truncated = true;
            self.first_line_exceeds_limit = line_index == 0 && !line.is_empty();
            return;
        }

        let remaining = self.max_bytes.saturating_sub(self.content.len());
        if piece_bytes <= remaining {
            if separator_len > 0 {
                self.content.push('\n');
            }
            self.content.push_str(line);
            self.output_lines = self.output_lines.saturating_add(1);
            return;
        }

        self.truncated = true;
        if line_index == 0 && line.len() > self.max_bytes {
            self.first_line_exceeds_limit = true;
        }

        let line_budget = if separator_len > 0 {
            if remaining == 0 {
                return;
            }
            self.content.push('\n');
            remaining - 1
        } else {
            remaining
        };

        let valid_bytes = utf8_prefix_len(line, line_budget);
        if valid_bytes > 0 {
            self.content.push_str(&line[..valid_bytes]);
            self.output_lines = self.output_lines.saturating_add(1);
            self.last_line_partial = valid_bytes < line.len();
        }
    }

    fn finish(self) -> TruncationResult {
        let output_bytes = self.content.len();
        TruncationResult {
            content: self.content,
            truncated: self.truncated,
            truncated_by: if self.truncated {
                Some(TruncatedBy::Bytes)
            } else {
                None
            },
            total_lines: self.total_lines,
            total_bytes: self.total_bytes,
            output_lines: self.output_lines,
            output_bytes,
            last_line_partial: self.last_line_partial,
            first_line_exceeds_limit: self.first_line_exceeds_limit,
            max_lines: usize::MAX,
            max_bytes: self.max_bytes,
        }
    }
}

fn push_escaped_utf8_for_line_output(escaped: &mut String, text: &str) {
    for character in text.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            character if character.is_control() => escaped.extend(character.escape_unicode()),
            character => escaped.push(character),
        }
    }
}

fn escape_bytes_for_line_output(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";

    let mut escaped = String::with_capacity(bytes.len());
    let mut remaining = bytes;
    while !remaining.is_empty() {
        match std::str::from_utf8(remaining) {
            Ok(valid) => {
                push_escaped_utf8_for_line_output(&mut escaped, valid);
                break;
            }
            Err(error) => {
                let valid_up_to = error.valid_up_to();
                if valid_up_to > 0 {
                    // SAFETY: `Utf8Error::valid_up_to` identifies a valid UTF-8 prefix.
                    let valid = std::str::from_utf8(&remaining[..valid_up_to])
                        .expect("valid_up_to must delimit valid UTF-8");
                    push_escaped_utf8_for_line_output(&mut escaped, valid);
                }

                let invalid_len = error
                    .error_len()
                    .unwrap_or_else(|| remaining.len().saturating_sub(valid_up_to));
                for &byte in &remaining[valid_up_to..valid_up_to + invalid_len] {
                    escaped.push_str("\\x");
                    escaped.push(char::from(HEX[usize::from(byte >> 4)]));
                    escaped.push(char::from(HEX[usize::from(byte & 0x0f)]));
                }
                remaining = &remaining[valid_up_to + invalid_len..];
            }
        }
    }
    escaped
}

fn path_for_line_output(path: &Path) -> String {
    #[cfg(windows)]
    {
        let rendered = path.to_string_lossy().replace('\\', "/");
        return escape_bytes_for_line_output(rendered.as_bytes());
    }

    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        escape_bytes_for_line_output(path.as_os_str().as_bytes())
    }

    #[cfg(not(any(unix, windows)))]
    escape_bytes_for_line_output(path.to_string_lossy().as_bytes())
}

fn compare_paths_for_line_output(a: &Path, b: &Path) -> Ordering {
    let a_rendered = path_for_line_output(a);
    let b_rendered = path_for_line_output(b);
    a_rendered
        .to_lowercase()
        .cmp(&b_rendered.to_lowercase())
        .then_with(|| a_rendered.cmp(&b_rendered))
}

fn strip_ansi_sequences(bytes: &[u8]) -> Vec<u8> {
    fn skip_csi(bytes: &[u8], mut index: usize) -> usize {
        while index < bytes.len() {
            let byte = bytes[index];
            index += 1;
            if (0x40..=0x7e).contains(&byte) {
                break;
            }
        }
        index
    }

    fn skip_string_sequence(bytes: &[u8], mut index: usize, osc: bool) -> usize {
        while index < bytes.len() {
            if osc && bytes[index] == 0x07 {
                return index + 1;
            }
            if bytes[index] == 0x1b && bytes.get(index + 1) == Some(&b'\\') {
                return index + 2;
            }
            index += 1;
        }
        index
    }

    let mut sanitized = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            0x1b => {
                index += 1;
                match bytes.get(index).copied() {
                    Some(b'[') => index = skip_csi(bytes, index + 1),
                    Some(b']') => index = skip_string_sequence(bytes, index + 1, true),
                    Some(b'P' | b'X' | b'^' | b'_') => {
                        index = skip_string_sequence(bytes, index + 1, false);
                    }
                    Some(_) => index += 1,
                    None => {}
                }
            }
            0x9b => index = skip_csi(bytes, index + 1),
            0x90 | 0x98 | 0x9e | 0x9f => {
                index = skip_string_sequence(bytes, index + 1, false);
            }
            0x9d => index = skip_string_sequence(bytes, index + 1, true),
            byte => {
                sanitized.push(byte);
                index += 1;
            }
        }
    }
    sanitized
}

fn diagnostic_for_line_output(bytes: &[u8], truncated: bool) -> String {
    let sanitized = strip_ansi_sequences(bytes);
    let Some(first) = sanitized
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
    else {
        return if truncated {
            "... [stderr truncated] ...".to_string()
        } else {
            String::new()
        };
    };
    let last = sanitized
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .expect("a non-whitespace byte exists");
    let mut rendered = escape_bytes_for_line_output(&sanitized[first..=last]);
    if truncated {
        rendered.push_str(" ... [stderr truncated] ...");
    }
    rendered
}

fn error_for_line_output(error: &impl std::fmt::Display) -> String {
    diagnostic_for_line_output(error.to_string().as_bytes(), false)
}

fn utf8_prefix_len(s: &str, max_bytes: usize) -> usize {
    let mut valid_bytes = max_bytes.min(s.len());
    while valid_bytes > 0 && !s.is_char_boundary(valid_bytes) {
        valid_bytes -= 1;
    }
    valid_bytes
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ToolOutputArtifactRef {
    schema: &'static str,
    id: String,
    tool_name: String,
    source_kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
    path: String,
    metadata_path: String,
    sha256: String,
    byte_count: u64,
    line_count: usize,
    preview_bytes: usize,
    content_type: &'static str,
    retention_class: &'static str,
    spillover_reason: &'static str,
    redaction_summary: ToolOutputArtifactRedactionSummary,
    safe_delete_candidate: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ToolOutputArtifactRedactionSummary {
    policy: &'static str,
    status: &'static str,
    redacted_count: usize,
    fields: Vec<String>,
    raw_secret_bytes_emitted: usize,
    binary_suspect: bool,
    max_redaction_bytes: u64,
}

struct RedactedToolOutputArtifact {
    bytes: Vec<u8>,
    summary: ToolOutputArtifactRedactionSummary,
}

fn tool_output_artifact_root() -> PathBuf {
    std::env::var_os("PI_TOOL_OUTPUT_ARTIFACT_DIR").map_or_else(
        || Config::global_dir().join("tool-output-artifacts"),
        PathBuf::from,
    )
}

static TOOL_OUTPUT_ARTIFACT_SESSIONS: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

fn tool_output_artifact_sessions() -> &'static Mutex<HashMap<String, String>> {
    TOOL_OUTPUT_ARTIFACT_SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) struct ToolOutputArtifactSessionGuard {
    tool_call_id: String,
    previous_session_id: Option<String>,
    active: bool,
}

impl Drop for ToolOutputArtifactSessionGuard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let Ok(mut sessions) = tool_output_artifact_sessions().lock() else {
            return;
        };
        if let Some(previous) = self.previous_session_id.take() {
            sessions.insert(self.tool_call_id.clone(), previous);
        } else {
            sessions.remove(&self.tool_call_id);
        }
    }
}

pub(crate) fn register_tool_output_artifact_session(
    tool_call_id: &str,
    session_id: &str,
) -> ToolOutputArtifactSessionGuard {
    if session_id.is_empty() {
        return ToolOutputArtifactSessionGuard {
            tool_call_id: String::new(),
            previous_session_id: None,
            active: false,
        };
    }
    let previous_session_id = tool_output_artifact_sessions()
        .lock()
        .ok()
        .and_then(|mut sessions| sessions.insert(tool_call_id.to_string(), session_id.to_string()));
    ToolOutputArtifactSessionGuard {
        tool_call_id: tool_call_id.to_string(),
        previous_session_id,
        active: true,
    }
}

fn tool_output_artifact_session_id(tool_call_id: &str) -> Option<String> {
    tool_output_artifact_sessions()
        .lock()
        .ok()
        .and_then(|sessions| sessions.get(tool_call_id).cloned())
}

fn tool_output_artifact_scope_dir(root: &Path, tool_call_id: &str) -> (PathBuf, Option<String>) {
    let call_scope = sanitize_artifact_scope(tool_call_id);
    if let Some(session_id) = tool_output_artifact_session_id(tool_call_id) {
        (
            root.join(sanitize_artifact_scope(&session_id))
                .join(call_scope),
            Some(session_id),
        )
    } else {
        (root.join(call_scope), None)
    }
}

fn sanitize_artifact_scope(scope: &str) -> String {
    let mut out = String::new();
    for ch in scope.chars().take(96) {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.trim_matches('_').is_empty() {
        "tool-call".to_string()
    } else {
        out
    }
}

fn artifact_line_count(bytes: &[u8]) -> usize {
    if bytes.is_empty() {
        0
    } else {
        memchr::memchr_iter(b'\n', bytes).count() + usize::from(!bytes.ends_with(b"\n"))
    }
}

fn artifact_details_object(
    details: &mut Option<serde_json::Value>,
) -> &mut serde_json::Map<String, serde_json::Value> {
    let value = details.get_or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if !value.is_object() {
        *value = serde_json::Value::Object(serde_json::Map::new());
    }
    value
        .as_object_mut()
        .expect("details value forced to object")
}

fn normalize_redaction_field(field: &str) -> String {
    let mut out = String::new();
    let mut previous_underscore = false;
    for ch in field.chars() {
        let normalized = if ch.is_ascii_alphanumeric() {
            previous_underscore = false;
            ch.to_ascii_lowercase()
        } else if previous_underscore {
            continue;
        } else {
            previous_underscore = true;
            '_'
        };
        out.push(normalized);
    }
    out.trim_matches('_').to_string()
}

fn record_redacted_field(fields: &mut Vec<String>, field: &str) {
    let field = normalize_redaction_field(field);
    if !field.is_empty() && !fields.iter().any(|existing| existing == &field) {
        fields.push(field);
    }
}

fn artifact_sensitive_key_value_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(
            r#"(?i)\b([A-Za-z_][A-Za-z0-9_.-]*(?:api[_-]?key|token|secret|password|passwd|credential|authorization)[A-Za-z0-9_.-]*)(\s*[:=]\s*)("[^"\r\n]*"|'[^'\r\n]*'|[^\s,;}]+)"#,
        )
        .expect("valid artifact key-value redaction regex")
    })
}

fn artifact_bearer_token_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"(?i)\b(Bearer\s+)([A-Za-z0-9._~+/=-]{8,})")
            .expect("valid artifact bearer redaction regex")
    })
}

fn artifact_token_value_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(
            r"\b(sk-[A-Za-z0-9][A-Za-z0-9_-]{10,}|gh[pousr]_[A-Za-z0-9_]{10,}|AKIA[0-9A-Z]{12,})\b",
        )
        .expect("valid artifact token value redaction regex")
    })
}

fn redacted_literal_for_value(value: &str) -> &'static str {
    if value.starts_with('"') && value.ends_with('"') {
        "\"[REDACTED]\""
    } else if value.starts_with('\'') && value.ends_with('\'') {
        "'[REDACTED]'"
    } else {
        "[REDACTED]"
    }
}

fn redact_tool_output_artifact_text(
    text: &str,
    binary_suspect: bool,
) -> RedactedToolOutputArtifact {
    let mut fields = Vec::new();
    let mut redacted_count = 0usize;

    let redacted = artifact_sensitive_key_value_regex()
        .replace_all(text, |caps: &regex::Captures<'_>| {
            let key = caps.get(1).map_or("", |m| m.as_str());
            let sep = caps.get(2).map_or("", |m| m.as_str());
            let value = caps.get(3).map_or("", |m| m.as_str());
            if value == "[REDACTED]" || value == "\"[REDACTED]\"" || value == "'[REDACTED]'" {
                caps.get(0).map_or("", |m| m.as_str()).to_string()
            } else {
                redacted_count = redacted_count.saturating_add(1);
                record_redacted_field(&mut fields, key);
                format!("{key}{sep}{}", redacted_literal_for_value(value))
            }
        })
        .to_string();

    let redacted = artifact_bearer_token_regex()
        .replace_all(&redacted, |caps: &regex::Captures<'_>| {
            redacted_count = redacted_count.saturating_add(1);
            record_redacted_field(&mut fields, "authorization");
            let prefix = caps.get(1).map_or("", |m| m.as_str());
            format!("{prefix}[REDACTED]")
        })
        .to_string();

    let redacted = artifact_token_value_regex()
        .replace_all(&redacted, |_caps: &regex::Captures<'_>| {
            redacted_count = redacted_count.saturating_add(1);
            record_redacted_field(&mut fields, "tokenValue");
            "[REDACTED]".to_string()
        })
        .to_string();

    fields.sort();
    let raw_secret_bytes_emitted = estimate_raw_secret_bytes(&redacted);
    let summary = ToolOutputArtifactRedactionSummary {
        policy: TOOL_OUTPUT_ARTIFACT_REDACTION_POLICY_V1,
        status: if raw_secret_bytes_emitted > 0 {
            "unsafe"
        } else if redacted_count > 0 {
            "redacted"
        } else {
            "clean"
        },
        redacted_count,
        fields,
        raw_secret_bytes_emitted,
        binary_suspect,
        max_redaction_bytes: TOOL_OUTPUT_ARTIFACT_REDACTION_MAX_BYTES,
    };

    RedactedToolOutputArtifact {
        bytes: redacted.into_bytes(),
        summary,
    }
}

fn estimate_raw_secret_bytes(text: &str) -> usize {
    let key_value_bytes = artifact_sensitive_key_value_regex()
        .captures_iter(text)
        .filter_map(|caps| {
            let value = caps.get(3)?.as_str();
            if value == "[REDACTED]" || value == "\"[REDACTED]\"" || value == "'[REDACTED]'" {
                None
            } else {
                caps.get(0).map(|m| m.as_str().len())
            }
        })
        .sum::<usize>();
    let bearer_bytes = artifact_bearer_token_regex()
        .find_iter(text)
        .map(|m| m.as_str().len())
        .sum::<usize>();
    let token_bytes = artifact_token_value_regex()
        .find_iter(text)
        .map(|m| m.as_str().len())
        .sum::<usize>();
    key_value_bytes
        .saturating_add(bearer_bytes)
        .saturating_add(token_bytes)
}

fn redact_tool_output_artifact_bytes(bytes: &[u8]) -> std::io::Result<RedactedToolOutputArtifact> {
    let binary_suspect =
        memchr::memchr(b'\0', bytes).is_some() || std::str::from_utf8(bytes).is_err();
    let text = String::from_utf8_lossy(bytes);
    let redacted = redact_tool_output_artifact_text(text.as_ref(), binary_suspect);
    if redacted.summary.raw_secret_bytes_emitted > 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "artifact redaction failed closed: raw secret-looking bytes remain",
        ));
    }
    Ok(redacted)
}

fn ensure_artifact_path_under_root(root: &Path, path: &Path) -> std::io::Result<()> {
    if path.starts_with(root) {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "artifact path {} is outside artifact root {}",
                path.display(),
                root.display()
            ),
        ))
    }
}

fn write_artifact_file_if_absent(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut file) => {
            file.write_all(bytes)?;
            tolerate_fsync_refusal(file.sync_all(), "artifact file", path)?;
            Ok(())
        }
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(err) => Err(err),
    }
}

fn write_text_tool_output_artifact_at_root(
    root: &Path,
    tool_name: &str,
    tool_call_id: &str,
    source_kind: &str,
    full_text: &str,
    preview_bytes: usize,
) -> std::io::Result<ToolOutputArtifactRef> {
    let bytes = full_text.as_bytes();
    if bytes.len() > TOOL_OUTPUT_ARTIFACT_MAX_BYTES_USIZE {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "artifact source exceeds {} hard limit",
                format_size(TOOL_OUTPUT_ARTIFACT_MAX_BYTES_USIZE)
            ),
        ));
    }
    if bytes.len() > TOOL_OUTPUT_ARTIFACT_REDACTION_MAX_BYTES_USIZE {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "artifact source exceeds {} redaction limit",
                format_size(TOOL_OUTPUT_ARTIFACT_REDACTION_MAX_BYTES_USIZE)
            ),
        ));
    }
    let redacted = redact_tool_output_artifact_bytes(bytes)?;
    let bytes = redacted.bytes.as_slice();
    let sha256 = crate::package_manager::hex_encode(&sha2::Sha256::digest(bytes));
    let (scope_dir, session_id) = tool_output_artifact_scope_dir(root, tool_call_id);
    std::fs::create_dir_all(&scope_dir)?;

    let id = format!("tool-artifact-{}", &sha256[..16]);
    let content_path = scope_dir.join(format!("{sha256}.txt"));
    let metadata_path = scope_dir.join(format!("{sha256}.json"));
    ensure_artifact_path_under_root(root, &content_path)?;
    ensure_artifact_path_under_root(root, &metadata_path)?;
    write_artifact_file_if_absent(&content_path, bytes)?;

    let artifact = ToolOutputArtifactRef {
        schema: TOOL_OUTPUT_ARTIFACT_SCHEMA_V1,
        id,
        tool_name: tool_name.to_string(),
        source_kind: source_kind.to_string(),
        session_id,
        path: content_path.display().to_string(),
        metadata_path: metadata_path.display().to_string(),
        sha256,
        byte_count: bytes.len().try_into().unwrap_or(u64::MAX),
        line_count: artifact_line_count(bytes),
        preview_bytes,
        content_type: "text/plain; charset=utf-8",
        retention_class: TOOL_OUTPUT_ARTIFACT_RETENTION_CLASS,
        spillover_reason: TOOL_OUTPUT_ARTIFACT_SPILLOVER_REASON,
        redaction_summary: redacted.summary,
        safe_delete_candidate: true,
    };
    let metadata = serde_json::to_vec_pretty(&artifact).map_err(std::io::Error::other)?;
    write_artifact_file_if_absent(&metadata_path, &metadata)?;
    Ok(artifact)
}

fn copy_text_tool_output_artifact_from_path_at_root(
    root: &Path,
    tool_name: &str,
    tool_call_id: &str,
    source_kind: &str,
    source_path: &Path,
    preview_bytes: usize,
) -> std::io::Result<ToolOutputArtifactRef> {
    let metadata = std::fs::metadata(source_path)?;
    if metadata.len() > TOOL_OUTPUT_ARTIFACT_MAX_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "artifact source exceeds {} hard limit",
                format_size(TOOL_OUTPUT_ARTIFACT_MAX_BYTES_USIZE)
            ),
        ));
    }
    if metadata.len() > TOOL_OUTPUT_ARTIFACT_REDACTION_MAX_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "artifact source exceeds {} redaction limit",
                format_size(TOOL_OUTPUT_ARTIFACT_REDACTION_MAX_BYTES_USIZE)
            ),
        ));
    }

    let mut source = std::fs::File::open(source_path)?;
    let mut source_bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    source.read_to_end(&mut source_bytes)?;
    let redacted = redact_tool_output_artifact_bytes(&source_bytes)?;
    let bytes = redacted.bytes.as_slice();

    let sha256 = crate::package_manager::hex_encode(&sha2::Sha256::digest(bytes));
    let (scope_dir, session_id) = tool_output_artifact_scope_dir(root, tool_call_id);
    std::fs::create_dir_all(&scope_dir)?;
    let id = format!("tool-artifact-{}", &sha256[..16]);
    let content_path = scope_dir.join(format!("{sha256}.txt"));
    let metadata_path = scope_dir.join(format!("{sha256}.json"));
    ensure_artifact_path_under_root(root, &content_path)?;
    ensure_artifact_path_under_root(root, &metadata_path)?;
    write_artifact_file_if_absent(&content_path, bytes)?;

    let artifact = ToolOutputArtifactRef {
        schema: TOOL_OUTPUT_ARTIFACT_SCHEMA_V1,
        id,
        tool_name: tool_name.to_string(),
        source_kind: source_kind.to_string(),
        session_id,
        path: content_path.display().to_string(),
        metadata_path: metadata_path.display().to_string(),
        sha256,
        byte_count: bytes.len().try_into().unwrap_or(u64::MAX),
        line_count: artifact_line_count(bytes),
        preview_bytes,
        content_type: "text/plain; charset=utf-8",
        retention_class: TOOL_OUTPUT_ARTIFACT_RETENTION_CLASS,
        spillover_reason: TOOL_OUTPUT_ARTIFACT_SPILLOVER_REASON,
        redaction_summary: redacted.summary,
        safe_delete_candidate: true,
    };
    let metadata = serde_json::to_vec_pretty(&artifact).map_err(std::io::Error::other)?;
    write_artifact_file_if_absent(&metadata_path, &metadata)?;
    Ok(artifact)
}

fn append_tool_output_artifact_notice(output_text: &mut String, artifact: &ToolOutputArtifactRef) {
    let _ = write!(
        output_text,
        "\n\n[Full tool output artifact: {} ({} bytes, {} lines, sha256 {}). Use read on this path to inspect more.]",
        artifact.path, artifact.byte_count, artifact.line_count, artifact.sha256,
    );
}

fn append_artifact_source_line(full_text: &mut String, line: &str) {
    // Artifact redaction refuses inputs above this size. Stop retaining bytes
    // as soon as we cross that boundary instead of allowing a streaming grep
    // with a caller-supplied huge limit to grow memory without bound. The one
    // extra byte keeps the persistence path honest: it reports that the full
    // artifact exceeded its redaction limit.
    let capture_limit = TOOL_OUTPUT_ARTIFACT_REDACTION_MAX_BYTES_USIZE.saturating_add(1);
    if full_text.len() >= capture_limit {
        return;
    }
    if !full_text.is_empty() {
        full_text.push('\n');
    }
    let remaining = capture_limit.saturating_sub(full_text.len());
    if line.len() <= remaining {
        full_text.push_str(line);
        return;
    }

    let mut prefix_len = remaining;
    while prefix_len > 0 && !line.is_char_boundary(prefix_len) {
        prefix_len -= 1;
    }
    full_text.push_str(&line[..prefix_len]);
    while full_text.len() <= TOOL_OUTPUT_ARTIFACT_REDACTION_MAX_BYTES_USIZE {
        full_text.push('!');
    }
}

fn record_tool_output_artifact_error(
    output_text: &mut String,
    details: &mut Option<serde_json::Value>,
    error: &std::io::Error,
) {
    let _ = write!(
        output_text,
        "\n\n[Tool output artifact persistence failed: {error}. Showing the bounded preview only.]"
    );
    artifact_details_object(details).insert(
        "artifactError".to_string(),
        serde_json::json!({
            "schema": TOOL_OUTPUT_ARTIFACT_SCHEMA_V1,
            "message": error.to_string(),
        }),
    );
}

fn attach_text_artifact_if_needed_at_root(
    root: &Path,
    output_text: &mut String,
    details: &mut Option<serde_json::Value>,
    tool_name: &str,
    tool_call_id: &str,
    source_kind: &str,
    full_text: &str,
) -> bool {
    if full_text.len() <= TOOL_OUTPUT_ARTIFACT_THRESHOLD_BYTES {
        return false;
    }
    match write_text_tool_output_artifact_at_root(
        root,
        tool_name,
        tool_call_id,
        source_kind,
        full_text,
        output_text.len(),
    ) {
        Ok(artifact) => {
            append_tool_output_artifact_notice(output_text, &artifact);
            artifact_details_object(details).insert(
                "artifact".to_string(),
                serde_json::to_value(&artifact).expect("artifact ref serializes"),
            );
            true
        }
        Err(err) => {
            record_tool_output_artifact_error(output_text, details, &err);
            false
        }
    }
}

fn attach_text_artifact_if_needed(
    output_text: &mut String,
    details: &mut Option<serde_json::Value>,
    tool_name: &str,
    tool_call_id: &str,
    source_kind: &str,
    full_text: &str,
) -> bool {
    let root = tool_output_artifact_root();
    attach_text_artifact_if_needed_at_root(
        &root,
        output_text,
        details,
        tool_name,
        tool_call_id,
        source_kind,
        full_text,
    )
}

fn attach_text_artifact_if_needed_with_root(
    root: Option<&Path>,
    output_text: &mut String,
    details: &mut Option<serde_json::Value>,
    tool_name: &str,
    tool_call_id: &str,
    source_kind: &str,
    full_text: &str,
) -> bool {
    if let Some(root) = root {
        attach_text_artifact_if_needed_at_root(
            root,
            output_text,
            details,
            tool_name,
            tool_call_id,
            source_kind,
            full_text,
        )
    } else {
        attach_text_artifact_if_needed(
            output_text,
            details,
            tool_name,
            tool_call_id,
            source_kind,
            full_text,
        )
    }
}

fn attach_text_artifact_from_path_if_needed_at_root(
    root: &Path,
    output_text: &mut String,
    details: &mut Option<serde_json::Value>,
    tool_name: &str,
    tool_call_id: &str,
    source_kind: &str,
    source_path: &Path,
) -> bool {
    let Ok(metadata) = std::fs::metadata(source_path) else {
        return false;
    };
    if metadata.len() <= u64::try_from(TOOL_OUTPUT_ARTIFACT_THRESHOLD_BYTES).unwrap_or(u64::MAX) {
        return false;
    }
    match copy_text_tool_output_artifact_from_path_at_root(
        root,
        tool_name,
        tool_call_id,
        source_kind,
        source_path,
        output_text.len(),
    ) {
        Ok(artifact) => {
            append_tool_output_artifact_notice(output_text, &artifact);
            artifact_details_object(details).insert(
                "artifact".to_string(),
                serde_json::to_value(&artifact).expect("artifact ref serializes"),
            );
            true
        }
        Err(err) => {
            record_tool_output_artifact_error(output_text, details, &err);
            false
        }
    }
}

fn attach_text_artifact_from_path_if_needed(
    output_text: &mut String,
    details: &mut Option<serde_json::Value>,
    tool_name: &str,
    tool_call_id: &str,
    source_kind: &str,
    source_path: &Path,
) -> bool {
    let root = tool_output_artifact_root();
    attach_text_artifact_from_path_if_needed_at_root(
        &root,
        output_text,
        details,
        tool_name,
        tool_call_id,
        source_kind,
        source_path,
    )
}

fn attach_text_artifact_from_path_if_needed_with_root(
    root: Option<&Path>,
    output_text: &mut String,
    details: &mut Option<serde_json::Value>,
    tool_name: &str,
    tool_call_id: &str,
    source_kind: &str,
    source_path: &Path,
) -> bool {
    if let Some(root) = root {
        attach_text_artifact_from_path_if_needed_at_root(
            root,
            output_text,
            details,
            tool_name,
            tool_call_id,
            source_kind,
            source_path,
        )
    } else {
        attach_text_artifact_from_path_if_needed(
            output_text,
            details,
            tool_name,
            tool_call_id,
            source_kind,
            source_path,
        )
    }
}

const TOOL_OUTPUT_CACHE_MAX_ENTRIES: usize = 128;
const TOOL_OUTPUT_CACHE_MAX_BYTES: usize = 8 * 1024 * 1024;
const TOOL_OUTPUT_CACHE_MAX_ENTRY_BYTES: usize = DEFAULT_MAX_BYTES + 64 * 1024;
const TOOL_OUTPUT_CACHE_MAX_FINGERPRINT_FILES: usize = 2048;
const TOOL_OUTPUT_CACHE_MAX_FINGERPRINT_BYTES: u64 = 8 * 1024 * 1024;
const TOOL_OUTPUT_CACHE_MAX_FILE_HASH_BYTES: u64 = 2 * 1024 * 1024;
const GREP_CONTEXT_MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;
const GREP_CONTEXT_MAX_LINES: usize = 200_000;
const RECURSIVE_SCAN_IGNORE_CONTROL_MAX_BYTES: u64 = 8 * 1024 * 1024;

#[cfg(unix)]
fn opened_file_matches_metadata_snapshot(
    expected: &std::fs::Metadata,
    opened: &std::fs::Metadata,
) -> bool {
    use std::os::unix::fs::MetadataExt as _;

    expected.dev() == opened.dev() && expected.ino() == opened.ino()
}

#[cfg(windows)]
fn opened_file_matches_metadata_snapshot(
    expected: &std::fs::Metadata,
    opened: &std::fs::Metadata,
) -> bool {
    use std::os::windows::fs::MetadataExt as _;

    expected.creation_time() == opened.creation_time()
        && expected.file_size() == opened.file_size()
        && expected.last_write_time() == opened.last_write_time()
}

#[cfg(not(any(unix, windows)))]
fn opened_file_matches_metadata_snapshot(
    _expected: &std::fs::Metadata,
    _opened: &std::fs::Metadata,
) -> bool {
    true
}

fn open_regular_file_for_capped_read(path: &Path) -> std::io::Result<std::fs::File> {
    // Resolve an intentional terminal symlink once, then refuse a second
    // terminal link swap while opening the resolved target. NONBLOCK keeps a
    // concurrent regular-file-to-FIFO swap from hanging the agent.
    let resolved_path = std::fs::canonicalize(path)?;
    let expected_metadata = std::fs::metadata(&resolved_path)?;
    if !expected_metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} is not a regular file", path.display()),
        ));
    }

    #[cfg(unix)]
    ensure_ancestors_searchable_sync(&resolved_path)?;

    #[cfg(unix)]
    let file = {
        let descriptor = rustix::fs::open(
            &resolved_path,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::NONBLOCK,
            rustix::fs::Mode::empty(),
        )
        .map_err(std::io::Error::from)?;
        std::fs::File::from(descriptor)
    };

    #[cfg(windows)]
    let file = {
        use std::os::windows::fs::OpenOptionsExt as _;

        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&resolved_path)?
    };

    #[cfg(not(any(unix, windows)))]
    let file = std::fs::File::open(&resolved_path)?;

    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} is not a regular file", path.display()),
        ));
    }
    if !opened_file_matches_metadata_snapshot(&expected_metadata, &metadata) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} changed while opening", path.display()),
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;

        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "{} changed to a reparse point while opening",
                    path.display()
                ),
            ));
        }
    }
    ensure_effective_mode_access(
        &metadata,
        &resolved_path,
        UNIX_ACCESS_READ,
        "bounded file reading",
    )?;
    Ok(file)
}

fn open_regular_file_within_roots_with<F>(
    path: &Path,
    allowed_roots: &[PathBuf],
    before_open: F,
) -> std::io::Result<std::fs::File>
where
    F: FnOnce(),
{
    let lexical_path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let canonical_path = strip_unc_prefix(std::fs::canonicalize(&lexical_path)?);
    let within_allowed_root = allowed_roots.iter().any(|root| {
        std::fs::canonicalize(root)
            .map(strip_unc_prefix)
            .is_ok_and(|canonical_root| canonical_path.starts_with(canonical_root))
    });
    if !within_allowed_root {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "refusing to read outside the allowed roots: {}",
                lexical_path.display()
            ),
        ));
    }

    let access_context = EffectiveModeAccessContext::current()?;
    #[cfg(unix)]
    {
        ensure_ancestors_searchable_with_context_sync(&lexical_path, &access_context)?;
        ensure_ancestors_searchable_with_context_sync(&canonical_path, &access_context)?;
    }

    let expected_metadata = std::fs::metadata(&canonical_path)?;
    if !expected_metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} is not a regular file", lexical_path.display()),
        ));
    }
    access_context.ensure(
        &expected_metadata,
        &canonical_path,
        UNIX_ACCESS_READ,
        "scoped file reading",
    )?;

    before_open();

    #[cfg(unix)]
    let file = {
        let descriptor = rustix::fs::open(
            &lexical_path,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::NONBLOCK,
            rustix::fs::Mode::empty(),
        )
        .map_err(std::io::Error::from)?;
        std::fs::File::from(descriptor)
    };

    #[cfg(windows)]
    let file = {
        use std::os::windows::fs::OpenOptionsExt as _;

        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&lexical_path)?
    };

    #[cfg(not(any(unix, windows)))]
    let file = std::fs::File::open(&lexical_path)?;

    let opened_metadata = file.metadata()?;
    if !opened_metadata.is_file()
        || !opened_file_matches_metadata_snapshot(&expected_metadata, &opened_metadata)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("{} changed while opening", lexical_path.display()),
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;

        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        if opened_metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "{} changed to a reparse point while opening",
                    lexical_path.display()
                ),
            ));
        }
    }
    access_context.ensure(
        &opened_metadata,
        &lexical_path,
        UNIX_ACCESS_READ,
        "scoped file reading",
    )?;
    Ok(file)
}

fn open_scoped_regular_file_for_read_with<F>(
    path: &Path,
    cwd: &Path,
    before_open: F,
) -> std::io::Result<std::fs::File>
where
    F: FnOnce(),
{
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    open_regular_file_within_roots_with(&path, &[cwd.to_path_buf()], before_open)
}

fn read_scoped_file_capped_sync(
    path: &Path,
    cwd: &Path,
    max_bytes: u64,
) -> std::io::Result<Vec<u8>> {
    let file = open_scoped_regular_file_for_read_with(path, cwd, || {})?;
    let mut contents = Vec::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut contents)?;
    if u64::try_from(contents.len()).unwrap_or(u64::MAX) > max_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} exceeds {max_bytes} bytes", path.display()),
        ));
    }
    Ok(contents)
}

fn read_file_capped_within_roots_sync(
    path: &Path,
    allowed_roots: &[PathBuf],
    max_bytes: u64,
) -> std::io::Result<Vec<u8>> {
    let file = open_regular_file_within_roots_with(path, allowed_roots, || {})?;
    let mut contents = Vec::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut contents)?;
    if u64::try_from(contents.len()).unwrap_or(u64::MAX) > max_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} exceeds {max_bytes} bytes", path.display()),
        ));
    }
    Ok(contents)
}

fn read_file_capped_sync(path: &Path, max_bytes: u64) -> std::io::Result<Vec<u8>> {
    let file = open_regular_file_for_capped_read(path)?;
    let mut contents = Vec::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut contents)?;
    if u64::try_from(contents.len()).unwrap_or(u64::MAX) > max_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} exceeds {max_bytes} bytes", path.display()),
        ));
    }
    Ok(contents)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ToolCacheDependency {
    path: PathBuf,
    fingerprint: [u8; 32],
}

#[derive(Debug, Clone, Copy)]
enum ToolCacheFingerprintMode {
    FileContent,
    DirectoryImmediate,
    DirectoryRecursive,
}

#[derive(Debug, Clone)]
struct CachedToolOutput {
    deps: Vec<ToolCacheDependency>,
    output: ToolOutput,
    weight: usize,
    generation: u64,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ToolOutputCacheStats {
    hits: usize,
    misses: usize,
    inserts: usize,
    invalidations: usize,
    disabled: usize,
    side_effect_accesses: usize,
    side_effect_insert_attempts: usize,
}

#[derive(Debug, Default)]
struct ToolOutputCache {
    entries: HashMap<String, CachedToolOutput>,
    order: VecDeque<(String, u64)>,
    total_bytes: usize,
    generation: u64,
    #[cfg(test)]
    stats: ToolOutputCacheStats,
}

impl ToolOutputCache {
    fn get(&mut self, key: &str, deps: &[ToolCacheDependency]) -> Option<ToolOutput> {
        self.generation = self.generation.saturating_add(1);
        let generation = self.generation;
        #[cfg(test)]
        {
            if is_side_effect_tool_cache_key(key) {
                self.stats.side_effect_accesses = self.stats.side_effect_accesses.saturating_add(1);
            }
        }

        if self
            .entries
            .get(key)
            .is_some_and(|entry| entry.deps == deps)
        {
            let entry = self.entries.get_mut(key)?;
            entry.generation = generation;
            self.order.push_back((key.to_string(), generation));
            #[cfg(test)]
            {
                self.stats.hits = self.stats.hits.saturating_add(1);
            }
            return Some(entry.output.clone());
        }

        if let Some(removed) = self.entries.remove(key) {
            self.total_bytes = self.total_bytes.saturating_sub(removed.weight);
            #[cfg(test)]
            {
                self.stats.invalidations = self.stats.invalidations.saturating_add(1);
            }
        } else {
            #[cfg(test)]
            {
                self.stats.misses = self.stats.misses.saturating_add(1);
            }
        }

        None
    }

    fn insert(
        &mut self,
        key: String,
        deps: Vec<ToolCacheDependency>,
        output: ToolOutput,
        weight: usize,
    ) {
        if weight == 0 || weight > TOOL_OUTPUT_CACHE_MAX_ENTRY_BYTES {
            #[cfg(test)]
            {
                self.stats.disabled = self.stats.disabled.saturating_add(1);
            }
            return;
        }

        #[cfg(test)]
        {
            if is_side_effect_tool_cache_key(&key) {
                self.stats.side_effect_insert_attempts =
                    self.stats.side_effect_insert_attempts.saturating_add(1);
            }
        }

        if let Some(removed) = self.entries.remove(&key) {
            self.total_bytes = self.total_bytes.saturating_sub(removed.weight);
        }

        self.generation = self.generation.saturating_add(1);
        let generation = self.generation;
        self.total_bytes = self.total_bytes.saturating_add(weight);
        self.order.push_back((key.clone(), generation));
        self.entries.insert(
            key,
            CachedToolOutput {
                deps,
                output,
                weight,
                generation,
            },
        );
        #[cfg(test)]
        {
            self.stats.inserts = self.stats.inserts.saturating_add(1);
        }
        self.evict_to_limits();
    }

    fn evict_to_limits(&mut self) {
        while self.entries.len() > TOOL_OUTPUT_CACHE_MAX_ENTRIES
            || self.total_bytes > TOOL_OUTPUT_CACHE_MAX_BYTES
        {
            let Some((key, generation)) = self.order.pop_front() else {
                break;
            };
            if self
                .entries
                .get(&key)
                .is_some_and(|entry| entry.generation == generation)
                && let Some(removed) = self.entries.remove(&key)
            {
                self.total_bytes = self.total_bytes.saturating_sub(removed.weight);
            }
        }
    }
}

fn tool_output_cache() -> &'static Mutex<ToolOutputCache> {
    static CACHE: OnceLock<Mutex<ToolOutputCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(ToolOutputCache::default()))
}

fn lock_tool_output_cache() -> std::sync::MutexGuard<'static, ToolOutputCache> {
    tool_output_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn tool_cache_key(tool: &str, cwd: &Path, input: &serde_json::Value) -> String {
    let input_json = serde_json::to_string(input).unwrap_or_else(|_| input.to_string());
    format!("{tool}\0{}\0{input_json}", cwd.display())
}

#[cfg(test)]
fn is_side_effect_tool_cache_key(key: &str) -> bool {
    key.starts_with("write\0") || key.starts_with("edit\0") || key.starts_with("bash\0")
}

fn cached_tool_output(key: &str, deps: Option<&[ToolCacheDependency]>) -> Option<ToolOutput> {
    let deps = deps?;
    lock_tool_output_cache().get(key, deps)
}

fn cache_tool_output(key: String, deps: Option<Vec<ToolCacheDependency>>, output: &ToolOutput) {
    let Some(deps) = deps else {
        return;
    };
    if output.details.as_ref().is_some_and(|details| {
        details.as_object().is_some_and(|details| {
            details.contains_key("artifact") || details.contains_key("artifactError")
        })
    }) {
        return;
    }
    let Some(weight) = cacheable_tool_output_weight(output) else {
        return;
    };
    lock_tool_output_cache().insert(key, deps, output.clone(), weight);
}

fn stable_cache_dependency_for_path(
    path: &Path,
    mode: ToolCacheFingerprintMode,
    before_deps: Option<&[ToolCacheDependency]>,
) -> Option<Vec<ToolCacheDependency>> {
    let before_deps = before_deps?;
    let after_deps = cache_dependency_for_path(path, mode)?;
    (before_deps == after_deps.as_slice()).then_some(after_deps)
}

fn cache_dependencies_for_scan(
    path: &Path,
    cwd: &Path,
    mode: ToolCacheFingerprintMode,
    recursive_access: Option<RecursiveScanAccess>,
) -> Option<Vec<ToolCacheDependency>> {
    let mut dependencies = cache_dependency_for_path(path, mode)?;
    if let Some(access) = recursive_access {
        dependencies.extend(ignore_control_cache_dependencies(path, cwd, access)?);
    }
    Some(dependencies)
}

fn stable_cache_dependencies_for_scan(
    path: &Path,
    cwd: &Path,
    mode: ToolCacheFingerprintMode,
    recursive_access: Option<RecursiveScanAccess>,
    before_deps: Option<&[ToolCacheDependency]>,
) -> Option<Vec<ToolCacheDependency>> {
    let before_deps = before_deps?;
    let after_deps = cache_dependencies_for_scan(path, cwd, mode, recursive_access)?;
    (before_deps == after_deps.as_slice()).then_some(after_deps)
}

fn cache_dependencies_for_scoped_scan(
    root: &ScopedScanRoot,
    cwd_root: &ScopedScanRoot,
    mode: ToolCacheFingerprintMode,
    recursive_access: Option<RecursiveScanAccess>,
) -> Option<Vec<ToolCacheDependency>> {
    let io_path = root.io_path();
    let fingerprint = match mode {
        // File roots are read via the bare descriptor path; `io_path()` is
        // only traversable for directory roots.
        ToolCacheFingerprintMode::FileContent => fingerprint_file_content(&root.file_read_path())?,
        ToolCacheFingerprintMode::DirectoryImmediate => fingerprint_directory_immediate(&io_path)?,
        ToolCacheFingerprintMode::DirectoryRecursive => fingerprint_directory_recursive(&io_path)?,
    };
    let mut dependencies = vec![ToolCacheDependency {
        path: root.logical_path().to_path_buf(),
        fingerprint,
    }];
    if let Some(access) = recursive_access {
        let cwd_io_path = cwd_root.io_path();
        let mut controls = ignore_control_cache_dependencies(&io_path, &cwd_io_path, access)?;
        for control in &mut controls {
            if let Ok(relative) = control.path.strip_prefix(&io_path) {
                control.path = root.logical_path().join(relative);
            } else if let Ok(relative) = control.path.strip_prefix(&cwd_io_path) {
                control.path = cwd_root.logical_path().join(relative);
            }
        }
        dependencies.extend(controls);
    }
    Some(dependencies)
}

fn stable_cache_dependencies_for_scoped_scan(
    root: &ScopedScanRoot,
    cwd_root: &ScopedScanRoot,
    mode: ToolCacheFingerprintMode,
    recursive_access: Option<RecursiveScanAccess>,
    before_deps: Option<&[ToolCacheDependency]>,
) -> Option<Vec<ToolCacheDependency>> {
    let before_deps = before_deps?;
    let after_deps = cache_dependencies_for_scoped_scan(root, cwd_root, mode, recursive_access)?;
    (before_deps == after_deps.as_slice()).then_some(after_deps)
}

fn cacheable_tool_output_weight(output: &ToolOutput) -> Option<usize> {
    let mut weight = output
        .details
        .as_ref()
        .and_then(|details| serde_json::to_vec(details).ok())
        .map_or(0, |details| details.len());

    for block in &output.content {
        match block {
            ContentBlock::Text(text) => {
                weight = weight.saturating_add(text.text.len());
                if let Some(signature) = &text.text_signature {
                    weight = weight.saturating_add(signature.len());
                }
            }
            ContentBlock::Image(_)
            | ContentBlock::Thinking(_)
            | ContentBlock::RedactedThinking(_)
            | ContentBlock::ToolCall(_) => return None,
        }
    }

    Some(weight)
}

fn cache_dependency_for_path(
    path: &Path,
    mode: ToolCacheFingerprintMode,
) -> Option<Vec<ToolCacheDependency>> {
    let fingerprint = match mode {
        ToolCacheFingerprintMode::FileContent => fingerprint_file_content(path)?,
        ToolCacheFingerprintMode::DirectoryImmediate => fingerprint_directory_immediate(path)?,
        ToolCacheFingerprintMode::DirectoryRecursive => fingerprint_directory_recursive(path)?,
    };

    Some(vec![ToolCacheDependency {
        path: path.to_path_buf(),
        fingerprint,
    }])
}

fn cache_dependency_for_open_file(
    path: &Path,
    file: &std::fs::File,
) -> Option<Vec<ToolCacheDependency>> {
    Some(vec![ToolCacheDependency {
        path: path.to_path_buf(),
        fingerprint: fingerprint_open_file_content(file)?,
    }])
}

fn stable_cache_dependency_for_open_file(
    path: &Path,
    file: Option<&std::fs::File>,
    before_deps: Option<&[ToolCacheDependency]>,
) -> Option<Vec<ToolCacheDependency>> {
    let file = file?;
    let before_deps = before_deps?;
    let after_deps = cache_dependency_for_open_file(path, file)?;
    (before_deps == after_deps.as_slice()).then_some(after_deps)
}

fn fingerprint_file_content(path: &Path) -> Option<[u8; 32]> {
    ensure_ancestors_searchable_sync(path).ok()?;
    let file = open_regular_file_for_capped_read(path).ok()?;
    fingerprint_open_file_content(&file)
}

fn fingerprint_open_file_content(file: &std::fs::File) -> Option<[u8; 32]> {
    let before = file.metadata().ok()?;
    if !before.is_file() || before.len() > TOOL_OUTPUT_CACHE_MAX_FILE_HASH_BYTES {
        return None;
    }

    let bytes = read_open_file_capped_at(file, TOOL_OUTPUT_CACHE_MAX_FILE_HASH_BYTES).ok()?;
    if u64::try_from(bytes.len()).ok()? != before.len() {
        return None;
    }
    let after = file.metadata().ok()?;
    if !cache_fingerprint_metadata_stable(&before, &after) {
        return None;
    }

    let mut hasher = sha2::Sha256::new();
    update_fingerprint_metadata(&mut hasher, Path::new(""), &before);
    hasher.update(sha2::Sha256::digest(&bytes));
    Some(hasher.finalize().into())
}

fn cache_fingerprint_metadata_stable(
    before: &std::fs::Metadata,
    after: &std::fs::Metadata,
) -> bool {
    opened_file_matches_metadata_snapshot(before, after)
        && before.is_file() == after.is_file()
        && before.is_dir() == after.is_dir()
        && before.file_type().is_symlink() == after.file_type().is_symlink()
        && before.len() == after.len()
        && before.modified().ok() == after.modified().ok()
}

fn read_open_file_capped_at(file: &std::fs::File, max_bytes: u64) -> std::io::Result<Vec<u8>> {
    let max_len = usize::try_from(max_bytes)
        .map_err(|_| std::io::Error::other("cache fingerprint limit exceeds usize"))?;
    let read_limit = max_len
        .checked_add(1)
        .ok_or_else(|| std::io::Error::other("cache fingerprint limit overflow"))?;
    let initial_capacity = file
        .metadata()
        .ok()
        .and_then(|metadata| usize::try_from(metadata.len()).ok())
        .unwrap_or(0)
        .min(read_limit);
    let mut contents = Vec::with_capacity(initial_capacity);
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut offset = 0_u64;

    while contents.len() < read_limit {
        let remaining = read_limit - contents.len();
        let chunk_len = remaining.min(buffer.len());
        let read = loop {
            match positioned_file_read(file, &mut buffer[..chunk_len], offset) {
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                result => break result?,
            }
        };
        if read == 0 {
            break;
        }
        contents.extend_from_slice(&buffer[..read]);
        offset = offset
            .checked_add(u64::try_from(read).unwrap_or(u64::MAX))
            .ok_or_else(|| std::io::Error::other("cache fingerprint offset overflow"))?;
    }

    if contents.len() > max_len {
        return Err(std::io::Error::other(
            "file exceeds cache fingerprint limit",
        ));
    }
    Ok(contents)
}

#[cfg(unix)]
fn positioned_file_read(
    file: &std::fs::File,
    buffer: &mut [u8],
    offset: u64,
) -> std::io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(file, buffer, offset)
}

#[cfg(windows)]
fn positioned_file_read(
    file: &std::fs::File,
    buffer: &mut [u8],
    offset: u64,
) -> std::io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(file, buffer, offset)
}

#[cfg(not(any(unix, windows)))]
fn positioned_file_read(
    file: &std::fs::File,
    buffer: &mut [u8],
    offset: u64,
) -> std::io::Result<usize> {
    let mut cloned = file.try_clone()?;
    std::io::Seek::seek(&mut cloned, std::io::SeekFrom::Start(offset))?;
    cloned.read(buffer)
}

fn fingerprint_directory_immediate(path: &Path) -> Option<[u8; 32]> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_dir() {
        return None;
    }
    ensure_ancestors_searchable_sync(path).ok()?;
    ensure_effective_mode_access(
        &metadata,
        path,
        UNIX_ACCESS_READ | UNIX_ACCESS_SEARCH,
        "cache directory scanning",
    )
    .ok()?;

    let mut entries = std::fs::read_dir(path)
        .ok()?
        .collect::<std::result::Result<Vec<_>, _>>()
        .ok()?;
    if entries.len() > TOOL_OUTPUT_CACHE_MAX_FINGERPRINT_FILES {
        return None;
    }
    entries.sort_by_key(std::fs::DirEntry::file_name);

    let mut hasher = sha2::Sha256::new();
    update_fingerprint_metadata(&mut hasher, Path::new(""), &metadata);
    for entry in entries {
        let entry_path = entry.path();
        let rel = entry.file_name();
        let rel = Path::new(&rel);
        let entry_metadata = std::fs::symlink_metadata(&entry_path).ok()?;
        update_fingerprint_metadata(&mut hasher, rel, &entry_metadata);
        if entry_metadata.file_type().is_symlink() {
            update_symlink_target(&mut hasher, &entry_path);
        }
    }

    Some(hasher.finalize().into())
}

fn fingerprint_directory_recursive(path: &Path) -> Option<[u8; 32]> {
    let metadata = std::fs::metadata(path).ok()?;
    if metadata.is_file() {
        return fingerprint_file_content(path);
    }
    if !metadata.is_dir() {
        return None;
    }
    ensure_ancestors_searchable_sync(path).ok()?;
    ensure_effective_mode_access(
        &metadata,
        path,
        UNIX_ACCESS_READ | UNIX_ACCESS_SEARCH,
        "cache directory scanning",
    )
    .ok()?;

    let mut budget = FingerprintBudget::default();
    let mut hasher = sha2::Sha256::new();
    update_fingerprint_metadata(&mut hasher, Path::new(""), &metadata);
    fingerprint_tree(path, path, &mut budget, &mut hasher)?;
    Some(hasher.finalize().into())
}

#[derive(Debug, Default)]
struct FingerprintBudget {
    entries: usize,
    bytes: u64,
}

fn fingerprint_tree(
    root: &Path,
    dir: &Path,
    budget: &mut FingerprintBudget,
    hasher: &mut sha2::Sha256,
) -> Option<()> {
    let dir_metadata = std::fs::metadata(dir).ok()?;
    ensure_effective_mode_access(
        &dir_metadata,
        dir,
        UNIX_ACCESS_READ | UNIX_ACCESS_SEARCH,
        "cache directory scanning",
    )
    .ok()?;
    let mut entries = std::fs::read_dir(dir)
        .ok()?
        .collect::<std::result::Result<Vec<_>, _>>()
        .ok()?;
    entries.sort_by_key(std::fs::DirEntry::path);

    for entry in entries {
        budget.entries = budget.entries.saturating_add(1);
        if budget.entries > TOOL_OUTPUT_CACHE_MAX_FINGERPRINT_FILES {
            return None;
        }

        let entry_path = entry.path();
        let rel = entry_path.strip_prefix(root).unwrap_or(&entry_path);
        let metadata = std::fs::symlink_metadata(&entry_path).ok()?;
        update_fingerprint_metadata(hasher, rel, &metadata);

        if metadata.file_type().is_symlink() {
            update_symlink_target(hasher, &entry_path);
        } else if metadata.is_dir() {
            fingerprint_tree(root, &entry_path, budget, hasher)?;
        } else if metadata.is_file() {
            if metadata.len() > TOOL_OUTPUT_CACHE_MAX_FILE_HASH_BYTES {
                return None;
            }
            ensure_effective_mode_access(
                &metadata,
                &entry_path,
                UNIX_ACCESS_READ,
                "cache file reading",
            )
            .ok()?;
            budget.bytes = budget.bytes.saturating_add(metadata.len());
            if budget.bytes > TOOL_OUTPUT_CACHE_MAX_FINGERPRINT_BYTES {
                return None;
            }
            let bytes =
                read_file_capped_sync(&entry_path, TOOL_OUTPUT_CACHE_MAX_FILE_HASH_BYTES).ok()?;
            hasher.update(sha2::Sha256::digest(&bytes));
        }
    }

    Some(())
}

fn update_fingerprint_metadata(
    hasher: &mut sha2::Sha256,
    path: &Path,
    metadata: &std::fs::Metadata,
) {
    hasher.update(path.to_string_lossy().as_bytes());
    hasher.update([0]);
    let file_type = metadata.file_type();
    hasher.update([
        u8::from(metadata.is_file()),
        u8::from(metadata.is_dir()),
        u8::from(file_type.is_symlink()),
    ]);
    hasher.update(metadata.len().to_le_bytes());
    let modified_nanos = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |duration| duration.as_nanos());
    hasher.update(modified_nanos.to_le_bytes());
    hasher.update([0xff]);
}

fn update_symlink_target(hasher: &mut sha2::Sha256, path: &Path) {
    if let Ok(target) = std::fs::read_link(path) {
        hasher.update(target.to_string_lossy().as_bytes());
    }
    hasher.update([0xfe]);
}

#[cfg(test)]
fn reset_tool_output_cache_for_tests() {
    *lock_tool_output_cache() = ToolOutputCache::default();
}

#[cfg(test)]
fn tool_output_cache_stats_for_tests() -> ToolOutputCacheStats {
    lock_tool_output_cache().stats
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

#[cfg(test)]
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

#[cfg(test)]
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
    if am_pm_variant.ne(resolved_str) {
        let candidate = PathBuf::from(&am_pm_variant);
        if candidate.starts_with(&normalized_cwd) && file_exists(&candidate) {
            return candidate;
        }
    }

    let nfd_variant = try_nfd_variant(resolved_str);
    if nfd_variant.ne(resolved_str) {
        let candidate = PathBuf::from(&nfd_variant);
        if candidate.starts_with(&normalized_cwd) && file_exists(&candidate) {
            return candidate;
        }
    }

    let curly_variant = try_curly_quote_variant(resolved_str);
    if curly_variant.ne(resolved_str) {
        let candidate = PathBuf::from(&curly_variant);
        if candidate.starts_with(&normalized_cwd) && file_exists(&candidate) {
            return candidate;
        }
    }

    let nfd_curly_variant = try_curly_quote_variant(&nfd_variant);
    if nfd_curly_variant.ne(resolved_str) {
        let candidate = PathBuf::from(&nfd_curly_variant);
        if candidate.starts_with(&normalized_cwd) && file_exists(&candidate) {
            return candidate;
        }
    }

    resolved
}

fn enforce_cwd_scope(
    path: &Path,
    cwd: &Path,
    action: &str,
    workspace: &WorkspaceHandle,
) -> Result<PathBuf> {
    let canonical_path = crate::extensions::safe_canonicalize(path);
    let canonical_cwd = crate::extensions::safe_canonicalize(cwd);
    let roots = workspace.snapshot_or(cwd);
    if roots.additional().is_empty() {
        // Single-root sessions keep the legacy decision and message verbatim.
        if !canonical_path.starts_with(&canonical_cwd) {
            return Err(Error::validation(format!(
                "Cannot {action} outside the working directory (resolved: {}, cwd: {})",
                path_for_line_output(&canonical_path),
                path_for_line_output(&canonical_cwd)
            )));
        }
        return Ok(canonical_path);
    }
    ensure_canonical_path_allowed(&canonical_path, &roots.all(), action)?;
    Ok(canonical_path)
}

struct ScopedScanRoot {
    logical_path: PathBuf,
    #[cfg(unix)]
    handle: std::fs::File,
    #[cfg(windows)]
    _component_guards: Vec<std::fs::File>,
}

impl ScopedScanRoot {
    fn logical_path(&self) -> &Path {
        &self.logical_path
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn io_path(&self) -> PathBuf {
        use std::os::fd::AsRawFd as _;

        let descriptor = self.handle.as_raw_fd();
        PathBuf::from("/proc/self/fd")
            .join(descriptor.to_string())
            .join(".")
    }

    // macOS/BSD `/dev/fd/N` entries support only re-open (dup) semantics:
    // traversing through them as directories fails (`/dev/fd/N/.` -> ENOENT,
    // readdir -> ENOTDIR), so fd-relative path IO is procfs-only. Fall back
    // to the canonicalized logical path, as Windows does; the pinned handle
    // still guards identity and liveness of the scan root.
    #[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
    fn io_path(&self) -> PathBuf {
        self.logical_path.clone()
    }

    #[cfg(windows)]
    fn io_path(&self) -> PathBuf {
        self.logical_path.clone()
    }

    // `&self` kept (despite being unused on this cfg) so both platform variants
    // share one method signature at every call site.
    #[allow(clippy::unused_self)]
    #[cfg(unix)]
    fn child_operand(&self) -> PathBuf {
        PathBuf::from(".")
    }

    #[allow(clippy::unused_self)]
    #[cfg(windows)]
    fn child_operand(&self) -> PathBuf {
        PathBuf::from(".")
    }

    /// Whether the pinned root is a regular file rather than a directory.
    fn is_file_root(&self) -> bool {
        #[cfg(unix)]
        {
            self.handle.metadata().is_ok_and(|m| m.is_file())
        }
        #[cfg(windows)]
        {
            std::fs::metadata(&self.logical_path).is_ok_and(|m| m.is_file())
        }
    }

    /// Path for reading the pinned root's bytes when it is a regular file.
    ///
    /// `io_path()` appends `/.` for directory traversal, which never resolves
    /// for a file descriptor. The bare `/proc/self/fd/N` form does not work
    /// either: procfs descriptor entries are magic symlinks, and every scoped
    /// read helper opens results with `O_NOFOLLOW`. Downstream readers verify
    /// descriptor identity against this path themselves, so the logical path
    /// is the correct read handle on every platform.
    fn file_read_path(&self) -> PathBuf {
        self.logical_path.clone()
    }

    /// Scanner operand for a pinned regular-file root. Children cannot chdir
    /// into a file, so Unix children read the pinned descriptor from stdin
    /// (`-`), while Windows children receive the logical path.
    #[allow(clippy::unused_self)]
    #[cfg(unix)]
    fn file_child_operand(&self) -> PathBuf {
        PathBuf::from("-")
    }

    #[cfg(windows)]
    fn file_child_operand(&self) -> PathBuf {
        self.logical_path.clone()
    }

    /// Path by which a child can access this root after its handle is installed
    /// as stdin. Scanner children run with their pinned scan root as cwd, so
    /// stdin remains available for the independently pinned workspace root.
    // `&self` kept (despite being unused on this cfg) so both platform variants
    // share one method signature at every call site.
    #[allow(clippy::unused_self)]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn inherited_child_operand(&self) -> PathBuf {
        PathBuf::from("/proc/self/fd/0").join(".")
    }

    // See io_path(): /dev/fd nodes cannot be traversed on macOS/BSD, so a
    // child reaches the independently pinned workspace root by its logical
    // path while the inherited stdin descriptor keeps the root alive.
    #[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
    fn inherited_child_operand(&self) -> PathBuf {
        self.logical_path.clone()
    }

    #[cfg(windows)]
    fn inherited_child_operand(&self) -> PathBuf {
        self.logical_path.clone()
    }

    #[cfg(unix)]
    fn child_stdin(&self) -> std::io::Result<Stdio> {
        Ok(Stdio::from(self.handle.try_clone()?))
    }

    #[cfg(windows)]
    fn child_stdin(&self) -> std::io::Result<Stdio> {
        Ok(Stdio::null())
    }

    fn map_child_output(&self, child_path: &Path) -> std::io::Result<ScopedScanOutputPath> {
        // A regular-file root has exactly one possible result: the root
        // itself. Unix scanners read it from stdin (reporting `<stdin>`),
        // so their reported path never resolves against the root anyway.
        if self.is_file_root() {
            return Ok(ScopedScanOutputPath {
                read_path: self.file_read_path(),
                logical_path: self.logical_path.clone(),
                relative: PathBuf::new(),
            });
        }

        #[cfg(unix)]
        let relative = if child_path.is_absolute() {
            child_path
                .strip_prefix(self.io_path())
                .or_else(|_| child_path.strip_prefix(&self.logical_path))
                .map_err(|_| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "scanner returned a path outside its pinned root",
                    )
                })?
                .to_path_buf()
        } else {
            child_path.to_path_buf()
        };

        #[cfg(windows)]
        let relative = if child_path.is_absolute() {
            strip_unc_prefix(child_path.to_path_buf())
                .strip_prefix(strip_unc_prefix(self.logical_path.clone()))
                .map_err(|_| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "scanner returned a path outside its pinned root",
                    )
                })?
                .to_path_buf()
        } else {
            child_path.to_path_buf()
        };

        let relative = normalize_scanner_relative_path(&relative)?;
        Ok(ScopedScanOutputPath {
            read_path: self.io_path().join(&relative),
            logical_path: self.logical_path.join(&relative),
            relative,
        })
    }
}

struct ScopedScanOutputPath {
    read_path: PathBuf,
    logical_path: PathBuf,
    relative: PathBuf,
}

fn normalize_scanner_relative_path(path: &Path) -> std::io::Result<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::Normal(name) => normalized.push(name),
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "scanner returned a non-relative path component",
                ));
            }
        }
    }
    Ok(normalized)
}

async fn open_scoped_scan_root(
    path: &Path,
    cwd: &Path,
    allow_file: bool,
    after_open: Option<Arc<dyn Fn() + Send + Sync>>,
) -> std::io::Result<ScopedScanRoot> {
    let path = path.to_path_buf();
    let cwd = cwd.to_path_buf();
    asupersync::runtime::spawn_blocking_io(move || {
        open_scoped_scan_root_sync(&path, &cwd, allow_file, || {
            if let Some(after_open) = after_open {
                after_open();
            }
        })
    })
    .await
}

#[cfg(unix)]
fn open_scoped_scan_root_sync<F>(
    path: &Path,
    cwd: &Path,
    allow_file: bool,
    after_open: F,
) -> std::io::Result<ScopedScanRoot>
where
    F: FnOnce(),
{
    let canonical_cwd = std::fs::canonicalize(cwd)?;
    let canonical_path = std::fs::canonicalize(path)?;
    if !canonical_path.starts_with(&canonical_cwd) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "scan root escaped the working directory before it could be pinned",
        ));
    }
    ensure_ancestors_searchable_sync(&canonical_path)?;
    let expected = std::fs::symlink_metadata(&canonical_path)?;
    if expected.file_type().is_symlink() || !(expected.is_dir() || allow_file && expected.is_file())
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "scan root is not an allowed directory or regular file",
        ));
    }

    let mut flags = rustix::fs::OFlags::RDONLY
        | rustix::fs::OFlags::CLOEXEC
        | rustix::fs::OFlags::NOFOLLOW
        | rustix::fs::OFlags::NONBLOCK;
    if expected.is_dir() {
        flags |= rustix::fs::OFlags::DIRECTORY;
    }
    let descriptor = rustix::fs::open(&canonical_path, flags, rustix::fs::Mode::empty())
        .map_err(std::io::Error::from)?;
    let handle = std::fs::File::from(descriptor);
    let opened = handle.metadata()?;
    if !opened_file_matches_metadata_snapshot(&expected, &opened)
        || !(opened.is_dir() || allow_file && opened.is_file())
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "scan root changed while it was being pinned",
        ));
    }
    let required_access = if opened.is_dir() {
        UNIX_ACCESS_READ | UNIX_ACCESS_SEARCH
    } else {
        UNIX_ACCESS_READ
    };
    ensure_effective_mode_access(&opened, &canonical_path, required_access, "scoped scanning")?;
    after_open();
    Ok(ScopedScanRoot {
        logical_path: canonical_path,
        handle,
    })
}

#[cfg(windows)]
fn open_scoped_scan_root_sync<F>(
    path: &Path,
    cwd: &Path,
    allow_file: bool,
    after_open: F,
) -> std::io::Result<ScopedScanRoot>
where
    F: FnOnce(),
{
    use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};
    use std::path::Component;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;

    let canonical_cwd = strip_unc_prefix(std::fs::canonicalize(cwd)?);
    let canonical_path = strip_unc_prefix(std::fs::canonicalize(path)?);
    if !canonical_path.starts_with(&canonical_cwd) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "scan root escaped the working directory before it could be pinned",
        ));
    }

    let components = canonical_path.components().collect::<Vec<_>>();
    let mut current = PathBuf::new();
    let mut guards = Vec::new();
    for (index, component) in components.iter().enumerate() {
        match component {
            Component::Prefix(prefix) => {
                current.push(prefix.as_os_str());
                continue;
            }
            Component::RootDir => {
                current.push(component.as_os_str());
                continue;
            }
            Component::CurDir => continue,
            Component::ParentDir => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "scan roots must not contain parent components",
                ));
            }
            Component::Normal(name) => current.push(name),
        }

        let is_final = index + 1 == components.len();
        let expected = std::fs::symlink_metadata(&current)?;
        let expected_type_allowed = if is_final {
            expected.is_dir() || allow_file && expected.is_file()
        } else {
            expected.is_dir()
        };
        if !expected_type_allowed || expected.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "scan root traverses a non-directory or Windows reparse point",
            ));
        }
        let handle = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&current)?;
        let opened = handle.metadata()?;
        if opened.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || opened.creation_time() != expected.creation_time()
            || opened.is_dir() != expected.is_dir()
            || opened.is_file() != expected.is_file()
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "scan path component changed while it was being pinned",
            ));
        }
        guards.push(handle);
    }
    if guards.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "scan root did not contain an openable path component",
        ));
    }
    after_open();
    Ok(ScopedScanRoot {
        logical_path: canonical_path,
        _component_guards: guards,
    })
}

#[cfg(not(any(unix, windows)))]
fn open_scoped_scan_root_sync<F>(
    _path: &Path,
    _cwd: &Path,
    _allow_file: bool,
    _after_open: F,
) -> std::io::Result<ScopedScanRoot>
where
    F: FnOnce(),
{
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "scoped scanner root pinning is unsupported on this platform",
    ))
}

async fn std_metadata_async(path: &Path) -> std::io::Result<std::fs::Metadata> {
    let path = path.to_path_buf();
    asupersync::runtime::spawn_blocking_io(move || std::fs::metadata(path)).await
}

fn ensure_ancestors_searchable_with_context_sync(
    path: &Path,
    access_context: &EffectiveModeAccessContext,
) -> std::io::Result<()> {
    for directory in path.ancestors().skip(1) {
        if directory.as_os_str().is_empty() {
            continue;
        }
        let metadata = std::fs::metadata(directory)?;
        if !metadata.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotADirectory,
                format!("Not a directory: {}", directory.display()),
            ));
        }
        access_context.ensure(
            &metadata,
            directory,
            UNIX_ACCESS_SEARCH,
            "directory traversal",
        )?;
    }
    Ok(())
}

fn ensure_ancestors_searchable_sync(path: &Path) -> std::io::Result<()> {
    let access_context = EffectiveModeAccessContext::current()?;
    ensure_ancestors_searchable_with_context_sync(path, &access_context)
}

/// Backend for the `grep`/`find` tools (bd-cv653.1.5).
///
/// `Inproc` (default) searches with the same engines ripgrep is built from
/// (`ignore` walker + `grep-searcher`/`globset`), linked into the process so
/// no `rg`/`fd` binaries are required. `External` shells out to `rg`/`fd`
/// exactly as before — kept as a debugging escape hatch via the
/// `search_backend` setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum SearchBackend {
    #[default]
    Inproc,
    External,
}

pub(crate) fn search_backend_from_config(config: Option<&Config>) -> SearchBackend {
    match config
        .and_then(|config| config.search_backend.as_deref())
        .map(str::trim)
    {
        Some("external") => SearchBackend::External,
        Some("inproc" | "") | None => SearchBackend::Inproc,
        Some(other) => {
            tracing::warn!(
                "unknown search_backend setting '{other}' (expected 'inproc' or 'external'); using inproc"
            );
            SearchBackend::Inproc
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecursiveScanAccess {
    DirectoriesOnly,
    ReadableFiles,
}

impl RecursiveScanAccess {
    const fn custom_ignore_filename(self) -> &'static str {
        match self {
            Self::DirectoriesOnly => ".fdignore",
            Self::ReadableFiles => ".rgignore",
        }
    }
}

#[derive(Debug)]
struct RecursiveScanDenial {
    path: PathBuf,
    kind: std::io::ErrorKind,
    message: String,
}

fn ensure_ignore_control_readable_sync(
    path: &Path,
    access_context: &EffectiveModeAccessContext,
    operation: &str,
) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(());
        }
        Err(err) => return Err(err),
    }
    ensure_ancestors_searchable_with_context_sync(path, access_context)?;
    let target = std::fs::canonicalize(path)?;
    ensure_ancestors_searchable_with_context_sync(&target, access_context)?;
    let metadata = std::fs::metadata(path)?;
    if !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("ignore control is not a regular file: {}", path.display()),
        ));
    }
    if metadata.len() > RECURSIVE_SCAN_IGNORE_CONTROL_MAX_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "ignore control {} exceeds {} bytes",
                path.display(),
                RECURSIVE_SCAN_IGNORE_CONTROL_MAX_BYTES
            ),
        ));
    }
    access_context.ensure(&metadata, path, UNIX_ACCESS_READ, operation)
}

#[derive(Debug, Clone)]
struct GitGlobalConfigLocations {
    home_dir: Option<PathBuf>,
    xdg_config_home: Option<PathBuf>,
}

impl GitGlobalConfigLocations {
    fn current() -> Self {
        #[allow(deprecated)]
        let home_dir = std::env::home_dir();
        let xdg_config_home = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| home_dir.as_ref().map(|home| home.join(".config")));
        Self {
            home_dir,
            xdg_config_home,
        }
    }
}

fn fd_global_ignore_path() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        // fd uses etcetera's Windows base strategy, which resolves to the
        // roaming application-data directory rather than XDG_CONFIG_HOME.
        dirs::config_dir().map(|root| root.join("fd").join("ignore"))
    }

    #[cfg(not(windows))]
    {
        // fd deliberately uses the XDG strategy on both Unix and macOS. The
        // `dirs` crate instead returns `~/Library/Application Support` on
        // macOS, so using `dirs::config_dir` here would validate and cache a
        // different file from the one the child process actually consumes.
        fd_global_ignore_path_from_locations(&GitGlobalConfigLocations::current())
    }
}

#[cfg(not(windows))]
fn fd_global_ignore_path_from_locations(locations: &GitGlobalConfigLocations) -> Option<PathBuf> {
    locations
        .xdg_config_home
        .as_ref()
        .map(|root| root.join("fd").join("ignore"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GitGlobalIgnoreControls {
    consumed_config_paths: Vec<PathBuf>,
    ignore_path: Option<PathBuf>,
}

// Keep in-process config discovery bounded and aligned with the largest file
// the tool-output cache can fingerprint. The external ignore crate reads these
// configs without a cap, but doing that in the long-lived agent process would
// let an oversized user-controlled config exhaust memory before rg/fd starts.
const GIT_GLOBAL_CONFIG_MAX_BYTES: u64 = TOOL_OUTPUT_CACHE_MAX_FILE_HASH_BYTES;
const GIT_GLOBAL_IGNORE_PATH_MAX_BYTES: usize = 2 * 1024 * 1024;

fn parse_gitconfig_excludes_path(
    data: &[u8],
    home_dir: Option<&Path>,
) -> std::io::Result<Option<PathBuf>> {
    static RE: OnceLock<regex::bytes::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::bytes::Regex::new(r#"(?im-u)^\s*excludesfile\s*=\s*"?\s*(\S+?)\s*"?\s*$"#)
            .expect("valid git excludesFile regex")
    });
    let Some(candidate) = re
        .captures(data)
        .and_then(|captures| captures.get(1))
        .and_then(|capture| std::str::from_utf8(capture.as_bytes()).ok())
    else {
        return Ok(None);
    };
    let home = home_dir.map(Path::to_string_lossy);
    // ignore 0.4.25 deliberately replaces every `~`, not only a leading one.
    // Preserve that behavior while bounding its potential allocation growth.
    let tilde_count = candidate.bytes().filter(|byte| *byte == b'~').count();
    let expanded_len = home.as_ref().map_or(Some(candidate.len()), |home| {
        candidate
            .len()
            .checked_sub(tilde_count)?
            .checked_add(tilde_count.checked_mul(home.len())?)
    });
    if expanded_len.is_none_or(|len| len > GIT_GLOBAL_IGNORE_PATH_MAX_BYTES) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "expanded git global ignore path exceeds {GIT_GLOBAL_IGNORE_PATH_MAX_BYTES} bytes"
            ),
        ));
    }
    let expanded = home.map_or_else(
        || candidate.to_string(),
        |home| candidate.replace('~', &home),
    );
    Ok(Some(PathBuf::from(expanded)))
}

fn read_checked_ignore_control_if_present(
    path: &Path,
    access_context: &EffectiveModeAccessContext,
    operation: &str,
) -> std::io::Result<Option<Vec<u8>>> {
    match std::fs::symlink_metadata(path) {
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(None);
        }
        Err(err) => return Err(err),
        Ok(_) => {}
    }
    ensure_ignore_control_readable_sync(path, access_context, operation)?;
    read_file_capped_sync(path, GIT_GLOBAL_CONFIG_MAX_BYTES).map(Some)
}

fn resolve_git_global_ignore_controls(
    locations: &GitGlobalConfigLocations,
    access_context: &EffectiveModeAccessContext,
    operation: &str,
) -> std::io::Result<GitGlobalIgnoreControls> {
    let mut consumed_config_paths = Vec::with_capacity(2);

    if let Some(home_dir) = &locations.home_dir {
        let home_config = home_dir.join(".gitconfig");
        consumed_config_paths.push(home_config.clone());
        if let Some(contents) =
            read_checked_ignore_control_if_present(&home_config, access_context, operation)?
            && let Some(ignore_path) =
                parse_gitconfig_excludes_path(&contents, locations.home_dir.as_deref())?
        {
            return Ok(GitGlobalIgnoreControls {
                consumed_config_paths,
                ignore_path: Some(ignore_path),
            });
        }
    }

    if let Some(xdg_config_home) = &locations.xdg_config_home {
        let xdg_config = xdg_config_home.join("git").join("config");
        consumed_config_paths.push(xdg_config.clone());
        if let Some(contents) =
            read_checked_ignore_control_if_present(&xdg_config, access_context, operation)?
            && let Some(ignore_path) =
                parse_gitconfig_excludes_path(&contents, locations.home_dir.as_deref())?
        {
            return Ok(GitGlobalIgnoreControls {
                consumed_config_paths,
                ignore_path: Some(ignore_path),
            });
        }
    }

    Ok(GitGlobalIgnoreControls {
        consumed_config_paths,
        ignore_path: locations
            .xdg_config_home
            .as_ref()
            .map(|root| root.join("git").join("ignore")),
    })
}

fn ignore_control_path_from_command_cwd(path: PathBuf, cwd: &Path) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

fn ensure_directory_ignore_controls_access_sync(
    directory: &Path,
    operation: &str,
    access: RecursiveScanAccess,
    access_context: &EffectiveModeAccessContext,
) -> std::io::Result<()> {
    for filename in [".gitignore", ".ignore", access.custom_ignore_filename()] {
        ensure_ignore_control_readable_sync(&directory.join(filename), access_context, operation)?;
    }
    // With --no-require-git, rg/fd probe this conventional path even outside
    // a repository. A linked worktree uses a regular `.git` file, in which
    // case `.git/info/exclude` is simply absent (ENOTDIR), not a scan error.
    ensure_ignore_control_readable_sync(
        &directory.join(".git").join("info").join("exclude"),
        access_context,
        operation,
    )
}

fn ensure_recursive_ignore_controls_access_sync(
    path: &Path,
    cwd: &Path,
    operation: &str,
    access: RecursiveScanAccess,
    access_context: &EffectiveModeAccessContext,
) -> std::io::Result<Option<PathBuf>> {
    let mut directory = if path.is_dir() {
        Some(path)
    } else {
        path.parent()
    };
    while let Some(current) = directory {
        ensure_directory_ignore_controls_access_sync(current, operation, access, access_context)?;
        directory = current.parent();
    }

    let global_controls = resolve_git_global_ignore_controls(
        &GitGlobalConfigLocations::current(),
        access_context,
        operation,
    )?;
    let global_ignore = global_controls
        .ignore_path
        .map(|path| ignore_control_path_from_command_cwd(path, cwd));
    if let Some(global_ignore) = global_ignore.as_deref() {
        ensure_ignore_control_readable_sync(global_ignore, access_context, operation)?;
    }
    if access == RecursiveScanAccess::DirectoriesOnly
        && let Some(global_ignore) = fd_global_ignore_path()
    {
        ensure_ignore_control_readable_sync(&global_ignore, access_context, operation)?;
    }
    Ok(global_ignore)
}

fn recursive_scan_ignore_control_paths(
    path: &Path,
    cwd: &Path,
    access: RecursiveScanAccess,
) -> Vec<PathBuf> {
    let mut paths = std::collections::BTreeSet::new();
    let mut directory = if path.is_dir() {
        Some(path)
    } else {
        path.parent()
    };
    while let Some(current) = directory {
        for filename in [".gitignore", ".ignore", access.custom_ignore_filename()] {
            paths.insert(current.join(filename));
        }
        paths.insert(current.join(".git").join("info").join("exclude"));
        directory = current.parent();
    }

    // These are passed explicitly to rg/fd. Keep the lexical cwd spelling as
    // a dependency too: it can be a symlink even though `path` is canonical.
    paths.insert(cwd.join(".gitignore"));
    paths.insert(path.join(".gitignore"));

    if access == RecursiveScanAccess::DirectoriesOnly
        && let Some(global_ignore) = fd_global_ignore_path()
    {
        paths.insert(global_ignore);
    }

    paths.into_iter().collect()
}

fn ignore_control_cache_dependency(
    path: PathBuf,
    access_context: &EffectiveModeAccessContext,
    budget: &mut FingerprintBudget,
) -> Option<ToolCacheDependency> {
    budget.entries = budget.entries.saturating_add(1);
    if budget.entries > TOOL_OUTPUT_CACHE_MAX_FINGERPRINT_FILES {
        return None;
    }

    let mut hasher = sha2::Sha256::new();
    match std::fs::symlink_metadata(&path) {
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            hasher.update(b"missing");
        }
        Err(_) => return None,
        Ok(_) => {
            ensure_ignore_control_readable_sync(
                &path,
                access_context,
                "cache ignore-control reading",
            )
            .ok()?;
            let metadata = std::fs::metadata(&path).ok()?;
            if !metadata.is_file() || metadata.len() > TOOL_OUTPUT_CACHE_MAX_FILE_HASH_BYTES {
                return None;
            }
            budget.bytes = budget.bytes.saturating_add(metadata.len());
            if budget.bytes > TOOL_OUTPUT_CACHE_MAX_FINGERPRINT_BYTES {
                return None;
            }
            let bytes = read_file_capped_sync(&path, TOOL_OUTPUT_CACHE_MAX_FILE_HASH_BYTES).ok()?;
            hasher.update(b"file");
            update_fingerprint_metadata(&mut hasher, Path::new(""), &metadata);
            hasher.update(sha2::Sha256::digest(&bytes));
        }
    }

    Some(ToolCacheDependency {
        path,
        fingerprint: hasher.finalize().into(),
    })
}

fn ignore_control_cache_dependencies(
    path: &Path,
    cwd: &Path,
    access: RecursiveScanAccess,
) -> Option<Vec<ToolCacheDependency>> {
    let access_context = EffectiveModeAccessContext::current().ok()?;
    let mut budget = FingerprintBudget::default();
    let mut control_paths = recursive_scan_ignore_control_paths(path, cwd, access)
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
    let global_controls = resolve_git_global_ignore_controls(
        &GitGlobalConfigLocations::current(),
        &access_context,
        "cache global-git-config reading",
    )
    .ok()?;
    control_paths.extend(global_controls.consumed_config_paths);
    if let Some(global_ignore) = global_controls.ignore_path {
        control_paths.insert(ignore_control_path_from_command_cwd(global_ignore, cwd));
    }
    control_paths
        .into_iter()
        .map(|path| ignore_control_cache_dependency(path, &access_context, &mut budget))
        .collect()
}

fn recursive_scan_walk_builder(
    path: &Path,
    cwd: &Path,
    access: RecursiveScanAccess,
    glob: Option<&str>,
    global_git_ignore: Option<&Path>,
) -> std::io::Result<ignore::WalkBuilder> {
    let mut builder = ignore::WalkBuilder::new(path);
    builder
        .current_dir(cwd)
        .hidden(false)
        .parents(true)
        .ignore(true)
        .git_ignore(true)
        // `ignore` resolves a relative core.excludesFile against the process
        // cwd before `current_dir` takes effect. External rg/fd run in `cwd`,
        // so load the already mode-checked, cwd-resolved file explicitly.
        .git_global(false)
        .git_exclude(true)
        .require_git(false)
        .follow_links(false);
    builder.add_custom_ignore_filename(access.custom_ignore_filename());
    if let Some(global_ignore) = global_git_ignore
        && global_ignore.is_file()
        && let Some(err) = builder.add_ignore(global_ignore)
    {
        return Err(std::io::Error::other(format!(
            "failed to load git global ignore file {}: {err}",
            global_ignore.display()
        )));
    }
    if access == RecursiveScanAccess::DirectoriesOnly
        && let Some(global_ignore) = fd_global_ignore_path()
        && global_ignore.is_file()
        && let Some(err) = builder.add_ignore(&global_ignore)
    {
        return Err(std::io::Error::other(format!(
            "failed to load fd global ignore file {}: {err}",
            global_ignore.display()
        )));
    }

    // The external commands pass these two files via --ignore-file in
    // addition to native discovery. Explicit ignore files are interpreted
    // relative to the command cwd, which can exclude a different (larger)
    // surface than the same file discovered natively below `path`.
    let workspace_gitignore = cwd.join(".gitignore");
    if workspace_gitignore.exists()
        && let Some(err) = builder.add_ignore(&workspace_gitignore)
    {
        return Err(std::io::Error::other(format!(
            "failed to load explicit ignore file {}: {err}",
            workspace_gitignore.display()
        )));
    }
    let root_gitignore = path.join(".gitignore");
    if root_gitignore != workspace_gitignore
        && root_gitignore.exists()
        && let Some(err) = builder.add_ignore(&root_gitignore)
    {
        return Err(std::io::Error::other(format!(
            "failed to load explicit ignore file {}: {err}",
            root_gitignore.display()
        )));
    }
    if let Some(glob) = glob {
        // rg evaluates --glob relative to its process cwd, even when the
        // explicit search root is absolute. Rooting this at `path` reverses
        // the result for slash-containing globs (for example `src/a/*.rs`).
        let mut overrides = ignore::overrides::OverrideBuilder::new(cwd);
        overrides.add(glob).map_err(|err| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, err.to_string())
        })?;
        builder.overrides(overrides.build().map_err(|err| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, err.to_string())
        })?);
    }
    Ok(builder)
}

/// Resolve the git global ignore file for an in-process scan, mirroring how
/// the external scanners (and the cache-dependency tracker) consume it.
fn resolved_global_git_ignore_for_scan(cwd: &Path) -> std::io::Result<Option<PathBuf>> {
    let access_context = EffectiveModeAccessContext::current()?;
    let controls = resolve_git_global_ignore_controls(
        &GitGlobalConfigLocations::current(),
        &access_context,
        "in-process scan global-git-config reading",
    )?;
    Ok(controls
        .ignore_path
        .map(|path| ignore_control_path_from_command_cwd(path, cwd)))
}

/// In-process `find` backend (bd-cv653.1.5): walk the pinned scan root with
/// the same ignore surface the external `fd` invocation consumed and collect
/// scan-root-relative matches for the shared result pipeline.
///
/// Filename-shaped patterns (no `/`) match the entry name, path-shaped
/// patterns match the scan-root-relative path — the same split the external
/// backend implements via `--full-path` plus the host-side Override filter.
fn find_inproc_scan_sync(
    scan_root: &Path,
    operation_cwd: &Path,
    pattern: &str,
    path_shaped: bool,
    scan_limit: usize,
    cancelled: &std::sync::atomic::AtomicBool,
) -> std::io::Result<Vec<PathBuf>> {
    let filename_matcher = if path_shaped {
        None
    } else {
        Some(
            globset::Glob::new(pattern)
                .map_err(|err| {
                    std::io::Error::new(std::io::ErrorKind::InvalidInput, err.to_string())
                })?
                .compile_matcher(),
        )
    };
    let path_matcher = if path_shaped {
        let mut builder = ignore::overrides::OverrideBuilder::new(scan_root);
        builder.add(pattern).map_err(|err| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, err.to_string())
        })?;
        Some(builder.build().map_err(|err| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, err.to_string())
        })?)
    } else {
        None
    };

    let global_git_ignore = resolved_global_git_ignore_for_scan(operation_cwd)?;
    let builder = recursive_scan_walk_builder(
        scan_root,
        operation_cwd,
        RecursiveScanAccess::DirectoriesOnly,
        None,
        global_git_ignore.as_deref(),
    )?;

    let mut results = Vec::new();
    for entry in builder.build() {
        if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "scan cancelled",
            ));
        }
        let entry = match entry {
            Ok(entry) => entry,
            // fd reports unreadable entries on stderr and keeps walking;
            // the permission preflight already failed closed on anything
            // the effective mode model forbids.
            Err(err) => {
                tracing::debug!("in-process find skipped an unreadable entry: {err}");
                continue;
            }
        };
        let path = entry.path();
        if path == scan_root {
            continue;
        }
        let Ok(relative) = path.strip_prefix(scan_root) else {
            continue;
        };
        let is_dir = entry
            .file_type()
            .is_some_and(|file_type| file_type.is_dir());
        let matched = filename_matcher.as_ref().map_or_else(
            || {
                path_matcher
                    .as_ref()
                    .is_some_and(|matcher| matcher.matched(path, is_dir).is_whitelist())
            },
            |matcher| {
                path.file_name()
                    .is_some_and(|name| matcher.is_match(Path::new(name)))
            },
        );
        if matched {
            results.push(relative.to_path_buf());
            if results.len() >= scan_limit {
                break;
            }
        }
    }
    Ok(results)
}

/// Validate every visible filesystem object a recursive search could visit.
///
/// The `ignore` walker mirrors the external scanners' hidden-file, ignore-file,
/// parent-ignore, and no-symlink-following behavior. A grep glob is installed as
/// an override so files excluded by that glob do not cause false denials. Find
/// validates directories plus the ignore-control files that fd must read;
/// enumerating any other regular-file name does not require content access.
/// The entire visible tree is checked:
/// an output limit cannot prove which candidates an external scanner will open
/// before it reaches that limit.
fn ensure_recursive_scan_access_sync(
    path: &Path,
    cwd: &Path,
    operation: &str,
    access: RecursiveScanAccess,
    glob: Option<&str>,
) -> std::io::Result<()> {
    let access_context = EffectiveModeAccessContext::current()?;
    ensure_ancestors_searchable_with_context_sync(path, &access_context)?;
    let global_git_ignore = ensure_recursive_ignore_controls_access_sync(
        path,
        cwd,
        operation,
        access,
        &access_context,
    )?;

    let root_metadata = std::fs::metadata(path)?;
    if root_metadata.is_file() {
        return if access == RecursiveScanAccess::ReadableFiles {
            access_context.ensure(&root_metadata, path, UNIX_ACCESS_READ, operation)
        } else {
            Ok(())
        };
    }
    if !root_metadata.is_dir() {
        return Ok(());
    }
    access_context.ensure(
        &root_metadata,
        path,
        UNIX_ACCESS_READ | UNIX_ACCESS_SEARCH,
        operation,
    )?;

    let denied = std::sync::Arc::new(std::sync::Mutex::new(None::<RecursiveScanDenial>));
    let denied_from_filter = std::sync::Arc::clone(&denied);
    let operation_owned = operation.to_string();

    let mut builder =
        recursive_scan_walk_builder(path, cwd, access, glob, global_git_ignore.as_deref())?;

    builder.filter_entry(move |entry| {
        if entry.path_is_symlink() {
            return true;
        }
        let Some(file_type) = entry.file_type() else {
            return true;
        };
        let required = if file_type.is_dir() {
            UNIX_ACCESS_READ | UNIX_ACCESS_SEARCH
        } else if file_type.is_file() && access == RecursiveScanAccess::ReadableFiles {
            UNIX_ACCESS_READ
        } else {
            return true;
        };

        let result = std::fs::symlink_metadata(entry.path()).and_then(|metadata| {
            access_context
                .ensure(&metadata, entry.path(), required, &operation_owned)
                .and_then(|()| {
                    if file_type.is_dir() {
                        // Ignore controls are loaded before the walker yields
                        // their directory contents. Validate them here so a
                        // positive rg glob cannot hide a consumed control file
                        // from the permission filter.
                        ensure_directory_ignore_controls_access_sync(
                            entry.path(),
                            &operation_owned,
                            access,
                            &access_context,
                        )
                    } else {
                        Ok(())
                    }
                })
        });
        if let Err(err) = result {
            let mut slot = denied_from_filter
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if slot.is_none() {
                *slot = Some(RecursiveScanDenial {
                    path: entry.path().to_path_buf(),
                    kind: err.kind(),
                    message: err.to_string(),
                });
            }
            drop(slot);
            return false;
        }
        true
    });

    finish_recursive_scan_access_walk(builder.build(), denied.as_ref(), operation)
}

fn finish_recursive_scan_access_walk(
    mut walker: ignore::Walk,
    denied: &std::sync::Mutex<Option<RecursiveScanDenial>>,
    operation: &str,
) -> std::io::Result<()> {
    loop {
        let next = walker.next();
        let denial = denied
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(denial) = denial {
            tracing::debug!(
                denied_path = %denial.path.display(),
                operation,
                error = %denial.message,
                "recursive scan denied a descendant"
            );
            return Err(std::io::Error::new(
                denial.kind,
                format!("{}: {}", denial.path.display(), denial.message),
            ));
        }

        match next {
            None => break,
            Some(Ok(entry)) => {
                if let Some(err) = entry.error()
                    && err
                        .io_error()
                        .is_some_and(|io_err| io_err.kind() == std::io::ErrorKind::PermissionDenied)
                {
                    tracing::debug!(
                        denied_path = %entry.path().display(),
                        operation,
                        error = %err,
                        "recursive scan could not read an ignore control file"
                    );
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        format!("{}: {err}", entry.path().display()),
                    ));
                }
            }
            Some(Err(err)) => {
                let kind = err
                    .io_error()
                    .map_or(std::io::ErrorKind::Other, std::io::Error::kind);
                tracing::debug!(operation, error = %err, "recursive scan walk failed");
                return Err(std::io::Error::new(kind, err.to_string()));
            }
        }
    }

    Ok(())
}

async fn ensure_recursive_scan_access(
    path: &Path,
    cwd: &Path,
    operation: &'static str,
    access: RecursiveScanAccess,
    glob: Option<String>,
) -> std::io::Result<()> {
    let path = path.to_path_buf();
    let cwd = cwd.to_path_buf();
    asupersync::runtime::spawn_blocking_io(move || {
        ensure_recursive_scan_access_sync(&path, &cwd, operation, access, glob.as_deref())
    })
    .await
}

/// Require directory-search access on every existing ancestor of `path`.
///
/// `metadata(path)` succeeds for UID 0 even when an ancestor has mode `000`.
/// Checking the traversal chain keeps privileged and unprivileged tool
/// behavior aligned before the real operation performs its identity-specific
/// checks.
async fn ensure_ancestors_searchable(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let path = path.to_path_buf();
        asupersync::runtime::spawn_blocking_io(move || ensure_ancestors_searchable_sync(&path))
            .await?;
    }

    #[cfg(not(unix))]
    {
        let _ = path;
    }

    Ok(())
}

/// Preserve traversal checks for both the user-supplied spelling and the
/// canonical path used for scope enforcement. Canonicalization otherwise
/// erases a mode-denied lexical parent of a terminal symlink under UID 0.
async fn ensure_scan_path_ancestors_searchable(
    lexical_path: &Path,
    canonical_path: &Path,
) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let lexical_path = lexical_path.to_path_buf();
        let canonical_path = canonical_path.to_path_buf();
        asupersync::runtime::spawn_blocking_io(move || {
            let access_context = EffectiveModeAccessContext::current()?;
            ensure_ancestors_searchable_with_context_sync(&lexical_path, &access_context)?;
            ensure_ancestors_searchable_with_context_sync(&canonical_path, &access_context)
        })
        .await?;
    }

    #[cfg(not(unix))]
    {
        let _ = (lexical_path, canonical_path);
    }

    Ok(())
}

/// Require read, write, and search access on the nearest existing parent directory.
///
/// This check happens before `create_dir_all`, preventing a privileged process
/// from creating missing descendants beneath an explicitly non-writable
/// ancestor. Newly-created parents are covered by their normal creation mode;
/// read access is required because the durable atomic-write path opens and
/// synchronizes the parent directory after rename. The eventual create/rename
/// syscall is still the final authority.
async fn ensure_parent_allows_creation(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let mut candidate = path.parent();
        while let Some(directory) = candidate {
            match std_metadata_async(directory).await {
                Ok(metadata) => {
                    if !metadata.is_dir() {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::NotADirectory,
                            format!("Not a directory: {}", directory.display()),
                        ));
                    }
                    ensure_effective_mode_access(
                        &metadata,
                        directory,
                        UNIX_ACCESS_READ | UNIX_ACCESS_WRITE | UNIX_ACCESS_SEARCH,
                        "durable file creation",
                    )?;
                    return ensure_ancestors_searchable(directory).await;
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    candidate = directory.parent();
                }
                Err(err) => return Err(err),
            }
        }
    }

    #[cfg(not(unix))]
    {
        let _ = path;
    }

    Ok(())
}

/// Same scoping contract as `enforce_cwd_scope`, but also accepts paths under
/// the configured pi-agent directory (`Config::global_dir()`, default
/// `~/.pi/agent/`, override via `PI_CODING_AGENT_DIR`).
///
/// Read access is broadened so the model can fetch the bodies of skill files,
/// prompt templates, and other resources that ship under the agent dir
/// without needing to fall back to a `bash cat`. Write/edit/grep/find/list
/// stay strictly cwd-only — broadening write access would let a misbehaving
/// model persist instructions into the agent dir, which is a much higher-
/// risk surface than the read case warrants. See pi_agent_rust#71.
///
/// Symlink escapes remain blocked because `safe_canonicalize` resolves
/// symlinks before the prefix check, so e.g. `~/.pi/agent/skills/foo/SKILL.md`
/// pointing at `/etc/passwd` resolves to `/etc/passwd` and fails the prefix
/// test against both cwd and agent dir.
fn enforce_read_scope_with_roots(
    path: &Path,
    cwd: &Path,
    agent_dir: &Path,
    workspace: &WorkspaceHandle,
) -> Result<PathBuf> {
    let canonical_path = crate::extensions::safe_canonicalize(path);
    let canonical_cwd = crate::extensions::safe_canonicalize(cwd);
    let canonical_agent = crate::extensions::safe_canonicalize(agent_dir);
    let roots = workspace.snapshot_or(cwd);
    if roots.additional().is_empty() {
        // Single-root sessions keep the legacy decision and message verbatim.
        if canonical_path.starts_with(&canonical_cwd)
            || canonical_path.starts_with(&canonical_agent)
        {
            return Ok(canonical_path);
        }
        return Err(Error::validation(format!(
            "Cannot read outside the working directory or agent dir \
             (resolved: {}, cwd: {}, agent dir: {})",
            canonical_path.display(),
            canonical_cwd.display(),
            canonical_agent.display(),
        )));
    }

    // Multi-root: cwd + additional roots + agent dir (read broadening, #71).
    let mut all_roots = roots.all();
    all_roots.push(canonical_agent);
    ensure_canonical_path_allowed(&canonical_path, &all_roots, "read")?;
    Ok(canonical_path)
}

/// Convenience wrapper that pulls the agent dir from the active config.
fn enforce_read_scope(path: &Path, cwd: &Path, workspace: &WorkspaceHandle) -> Result<PathBuf> {
    let agent_dir = crate::config::Config::global_dir();
    enforce_read_scope_with_roots(path, cwd, &agent_dir, workspace)
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

/// Returns `true` when an `fsync`/`fdatasync` durability barrier was *refused*
/// by the filesystem rather than reflecting a real write failure.
///
/// The bytes are already handed to the kernel by the preceding `write(2)`;
/// `fsync` only asks the filesystem to make them durable. Some filesystems —
/// notably virtiofs / FUSE bind mounts (Docker Desktop for macOS) and various
/// network filesystems — do not implement `fsync` on a given descriptor and
/// report it with `EBADF`, `EINVAL`, or an "unsupported" error even though the
/// `write(2)` and the subsequent atomic `rename(2)` already landed the data
/// correctly. Failing the whole write tool in that case is wrong: the file is
/// complete and correct on disk. We downgrade these specific refusals to a
/// warning. Genuine I/O failures (`EIO`, `ENOSPC`, `EDQUOT`, …) still
/// propagate. See issue #136.
fn is_fsync_refused(err: &std::io::Error) -> bool {
    // EBADF = 9 and EINVAL = 22 on both Linux and macOS. `ErrorKind::Unsupported`
    // captures ENOTSUP/EOPNOTSUPP/ENOSYS portably without a `libc` dependency
    // (this crate is `#![forbid(unsafe_code)]`).
    matches!(err.raw_os_error(), Some(9 | 22)) || err.kind() == std::io::ErrorKind::Unsupported
}

/// Runs a durability `fsync` (`result`), treating a filesystem *refusal* (see
/// [`is_fsync_refused`]) as a non-fatal warning while still propagating real
/// I/O errors. `what` and `path` are used only for the diagnostic log line.
fn tolerate_fsync_refusal(
    result: std::io::Result<()>,
    what: &str,
    path: &Path,
) -> std::io::Result<()> {
    match result {
        Ok(()) => Ok(()),
        Err(err) if is_fsync_refused(&err) => {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "{what} fsync refused by filesystem (non-POSIX durability semantics); \
                 data already written, continuing without a durability barrier"
            );
            Ok(())
        }
        Err(err) => Err(err),
    }
}

#[cfg(all(unix, any(target_os = "espidf", target_os = "redox")))]
fn sync_parent_dir(path: &Path) -> std::io::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    // Directory fsync is a pure durability nicety (it makes the rename durable);
    // on filesystems that refuse it the rename is still visible, so tolerate a
    // refusal rather than failing the write. See issue #136.
    tolerate_fsync_refusal(
        std::fs::File::open(parent).and_then(|dir| dir.sync_all()),
        "parent directory",
        parent,
    )
}

#[cfg(not(unix))]
fn sync_parent_dir(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

fn set_atomic_replacement_permissions(
    temp_file: &tempfile::NamedTempFile,
    original_permissions: Option<std::fs::Permissions>,
) -> std::io::Result<()> {
    if let Some(permissions) = original_permissions {
        temp_file.as_file().set_permissions(permissions)?;
    } else {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            temp_file
                .as_file()
                .set_permissions(std::fs::Permissions::from_mode(0o644))?;
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AtomicContentExpectation {
    len: u64,
    sha256: [u8; 32],
}

impl AtomicContentExpectation {
    fn from_bytes(bytes: &[u8]) -> Self {
        Self {
            len: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            sha256: sha2::Sha256::digest(bytes).into(),
        }
    }
}

fn ensure_atomic_source_unchanged(
    mut source: std::fs::File,
    expectation: AtomicContentExpectation,
) -> std::io::Result<()> {
    let metadata = source.metadata()?;
    if !metadata.is_file() || metadata.len() != expectation.len {
        return Err(std::io::Error::other(
            "file changed since it was read; re-read it and retry the edit",
        ));
    }

    let mut hasher = sha2::Sha256::new();
    let mut total = 0_u64;
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = source.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
        if total > expectation.len {
            return Err(std::io::Error::other(
                "file changed since it was read; re-read it and retry the edit",
            ));
        }
        hasher.update(&buffer[..read]);
    }

    let actual_sha256: [u8; 32] = hasher.finalize().into();
    if total != expectation.len || actual_sha256 != expectation.sha256 {
        return Err(std::io::Error::other(
            "file changed since it was read; re-read it and retry the edit",
        ));
    }
    Ok(())
}

/// Normalize a raw `rustix` stat identity to platform-independent widths.
///
/// The libc stat field types differ per target (`st_dev` is `i32` and
/// `st_mode` is `u16` on macOS, `u64`/`u32` on Linux), while the comparisons
/// below mix them with `std::os::unix::fs::MetadataExt` values (`u64`) and
/// `Permissions::from_mode` (`u32`). Widening every field once here keeps the
/// identity checks compiling identically on every unix target.
#[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
#[allow(
    clippy::cast_lossless,
    clippy::cast_sign_loss,
    clippy::unnecessary_cast
)]
const fn stat_replacement_identity(metadata: &rustix::fs::Stat) -> (u64, u64, u32) {
    (
        metadata.st_dev as u64,
        metadata.st_ino as u64,
        metadata.st_mode as u32,
    )
}

// Keep the descriptor-pinned validation, write, rename, durability, and
// identity-checked cleanup sequence together: splitting this security-critical
// transaction across helpers would make its ordering invariants harder to audit.
#[allow(clippy::too_many_lines)]
/// `allowed_roots` is the workspace's canonical root set (primary cwd plus
/// any `/add-dir` roots); the replacement parent must live under one of
/// them. A single-root caller passes `&[cwd]`.
fn atomic_replace_file_with<F>(
    target: &Path,
    allowed_roots: &[PathBuf],
    contents: &[u8],
    expected_source: Option<AtomicContentExpectation>,
    before_persist: F,
) -> std::io::Result<()>
where
    F: FnOnce(),
{
    let parent = target.parent().unwrap_or_else(|| Path::new("."));

    #[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
    {
        use std::os::unix::fs::MetadataExt as _;

        let canonical_parent = std::fs::canonicalize(parent)?;
        if !atomic_parent_within_roots(&canonical_parent, allowed_roots) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "atomic replacement parent escaped the working directory: {}",
                    parent.display()
                ),
            ));
        }

        let access_context = EffectiveModeAccessContext::current()?;
        ensure_ancestors_searchable_with_context_sync(parent, &access_context)?;
        ensure_ancestors_searchable_with_context_sync(&canonical_parent, &access_context)?;
        let expected_parent_metadata = std::fs::metadata(&canonical_parent)?;
        if !expected_parent_metadata.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotADirectory,
                format!("Not a directory: {}", parent.display()),
            ));
        }
        access_context.ensure(
            &expected_parent_metadata,
            &canonical_parent,
            UNIX_ACCESS_READ | UNIX_ACCESS_WRITE | UNIX_ACCESS_SEARCH,
            "atomic file replacement",
        )?;

        let parent_descriptor = rustix::fs::open(
            parent,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        )
        .map_err(std::io::Error::from)?;
        let parent_file = std::fs::File::from(parent_descriptor);
        let opened_parent_metadata = parent_file.metadata()?;
        if !opened_file_matches_metadata_snapshot(
            &expected_parent_metadata,
            &opened_parent_metadata,
        ) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("{} changed while opening", parent.display()),
            ));
        }
        access_context.ensure(
            &opened_parent_metadata,
            parent,
            UNIX_ACCESS_READ | UNIX_ACCESS_WRITE | UNIX_ACCESS_SEARCH,
            "atomic file replacement",
        )?;

        let target_name = target
            .file_name()
            .ok_or_else(|| std::io::Error::other("replacement target has no basename"))?;
        let expected_target_identity = match rustix::fs::statat(
            &parent_file,
            target_name,
            rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
        ) {
            Ok(metadata)
                if rustix::fs::FileType::from_raw_mode(metadata.st_mode)
                    == rustix::fs::FileType::RegularFile =>
            {
                Some(stat_replacement_identity(&metadata))
            }
            Ok(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("{} is not a regular file", target.display()),
                ));
            }
            Err(err) if err == rustix::io::Errno::NOENT => None,
            Err(err) => return Err(std::io::Error::from(err)),
        };
        let original_permissions = expected_target_identity.map(|(_, _, mode)| {
            use std::os::unix::fs::PermissionsExt as _;

            std::fs::Permissions::from_mode(mode)
        });

        // The pathname stored by `NamedTempFile` is not descriptor-relative.
        // Disable its Drop cleanup from construction so a concurrent parent
        // rename cannot redirect cleanup into an attacker-controlled directory.
        // Errors below clean up only after re-validating the entry through the
        // pinned parent descriptor.
        let mut temp_file = tempfile::Builder::new()
            .disable_cleanup(true)
            .tempfile_in(parent)?;
        let temp_name = temp_file
            .path()
            .file_name()
            .ok_or_else(|| std::io::Error::other("temporary file has no basename"))?
            .to_os_string();
        let temp_descriptor_metadata = rustix::fs::fstat(temp_file.as_file())?;
        let temp_entry_metadata = rustix::fs::statat(
            &parent_file,
            &temp_name,
            rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
        )?;
        if temp_descriptor_metadata.st_dev != temp_entry_metadata.st_dev
            || temp_descriptor_metadata.st_ino != temp_entry_metadata.st_ino
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "temporary file was created outside the pinned parent directory",
            ));
        }

        let replacement_result = (|| {
            temp_file.as_file_mut().write_all(contents)?;
            set_atomic_replacement_permissions(&temp_file, original_permissions)?;
            tolerate_fsync_refusal(temp_file.as_file_mut().sync_all(), "temp file", target)?;

            before_persist();

            // A descriptor-relative rename is safe even if this check races,
            // but fail before mutation when a deterministic parent swap is
            // already visible. The post-rename check reports narrower races.
            let current_parent_metadata = std::fs::metadata(parent)?;
            if current_parent_metadata.dev() != opened_parent_metadata.dev()
                || current_parent_metadata.ino() != opened_parent_metadata.ino()
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!("{} changed before atomic replacement", parent.display()),
                ));
            }

            let current_target_identity = match rustix::fs::statat(
                &parent_file,
                target_name,
                rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
            ) {
                Ok(metadata)
                    if rustix::fs::FileType::from_raw_mode(metadata.st_mode)
                        == rustix::fs::FileType::RegularFile =>
                {
                    Some(stat_replacement_identity(&metadata))
                }
                Ok(_) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "atomic replacement target changed to a non-regular file",
                    ));
                }
                Err(err) if err == rustix::io::Errno::NOENT => None,
                Err(err) => return Err(std::io::Error::from(err)),
            };
            if current_target_identity != expected_target_identity {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "atomic replacement target changed before rename",
                ));
            }

            if let Some(expectation) = expected_source {
                let Some((expected_dev, expected_ino, _)) = expected_target_identity else {
                    return Err(std::io::Error::other(
                        "file changed since it was read; re-read it and retry the edit",
                    ));
                };
                let source_descriptor = rustix::fs::openat(
                    &parent_file,
                    target_name,
                    rustix::fs::OFlags::RDONLY
                        | rustix::fs::OFlags::CLOEXEC
                        | rustix::fs::OFlags::NOFOLLOW
                        | rustix::fs::OFlags::NONBLOCK,
                    rustix::fs::Mode::empty(),
                )
                .map_err(std::io::Error::from)?;
                let source_file = std::fs::File::from(source_descriptor);
                let source_metadata = source_file.metadata()?;
                if !source_metadata.is_file()
                    || source_metadata.dev() != expected_dev
                    || source_metadata.ino() != expected_ino
                {
                    return Err(std::io::Error::other(
                        "file changed since it was read; re-read it and retry the edit",
                    ));
                }
                access_context.ensure(
                    &source_metadata,
                    target,
                    UNIX_ACCESS_READ,
                    "optimistic edit validation",
                )?;
                ensure_atomic_source_unchanged(source_file, expectation)?;

                // Re-check the directory entry after hashing. This keeps a
                // rename/replacement concurrent with the digest read from
                // being overwritten by the final descriptor-relative rename.
                let after_digest = rustix::fs::statat(
                    &parent_file,
                    target_name,
                    rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
                )?;
                let after_identity = stat_replacement_identity(&after_digest);
                if after_identity.0 != expected_dev
                    || after_identity.1 != expected_ino
                    || Some(after_identity) != expected_target_identity
                {
                    return Err(std::io::Error::other(
                        "file changed since it was read; re-read it and retry the edit",
                    ));
                }
            }

            rustix::fs::renameat(&parent_file, &temp_name, &parent_file, target_name)?;

            let persisted_entry_metadata = rustix::fs::statat(
                &parent_file,
                target_name,
                rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
            )?;
            if temp_descriptor_metadata.st_dev != persisted_entry_metadata.st_dev
                || temp_descriptor_metadata.st_ino != persisted_entry_metadata.st_ino
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "atomic replacement target changed immediately after rename",
                ));
            }
            tolerate_fsync_refusal(parent_file.sync_all(), "parent directory", parent)?;

            let current_parent_metadata = std::fs::metadata(parent)?;
            if current_parent_metadata.dev() != opened_parent_metadata.dev()
                || current_parent_metadata.ino() != opened_parent_metadata.ino()
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!("{} changed during atomic replacement", parent.display()),
                ));
            }
            Ok(())
        })();

        if replacement_result.is_err()
            && let Ok(current_temp_entry) = rustix::fs::statat(
                &parent_file,
                &temp_name,
                rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
            )
            && current_temp_entry.st_dev == temp_descriptor_metadata.st_dev
            && current_temp_entry.st_ino == temp_descriptor_metadata.st_ino
        {
            // Only unlink the exact temporary inode we created. An attacker
            // that replaced the name cannot make cleanup delete their entry.
            let _ = rustix::fs::unlinkat(&parent_file, &temp_name, rustix::fs::AtFlags::empty());
        }

        replacement_result
    }

    #[cfg(not(all(unix, not(any(target_os = "espidf", target_os = "redox")))))]
    {
        let original_permissions = std::fs::metadata(target)
            .ok()
            .filter(std::fs::Metadata::is_file)
            .map(|metadata| metadata.permissions());
        let canonical_parent = strip_unc_prefix(std::fs::canonicalize(parent)?);
        if !atomic_parent_within_roots(&canonical_parent, allowed_roots) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "atomic replacement parent escaped the working directory: {}",
                    parent.display()
                ),
            ));
        }

        let mut temp_file = tempfile::NamedTempFile::new_in(parent)?;
        let canonical_temp_parent = temp_file
            .path()
            .parent()
            .and_then(|temp_parent| std::fs::canonicalize(temp_parent).ok())
            .map(strip_unc_prefix)
            .ok_or_else(|| std::io::Error::other("failed to resolve temporary-file parent"))?;
        if canonical_temp_parent != canonical_parent {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "temporary file was created outside the validated parent directory",
            ));
        }

        temp_file.as_file_mut().write_all(contents)?;
        set_atomic_replacement_permissions(&temp_file, original_permissions)?;
        tolerate_fsync_refusal(temp_file.as_file_mut().sync_all(), "temp file", target)?;

        before_persist();

        let parent_before_persist = strip_unc_prefix(std::fs::canonicalize(parent)?);
        if parent_before_persist != canonical_parent {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("{} changed before atomic replacement", parent.display()),
            ));
        }
        if let Some(expectation) = expected_source {
            let source = open_regular_file_within_roots_with(target, allowed_roots, || {})?;
            ensure_atomic_source_unchanged(source, expectation)?;

            let parent_after_digest = strip_unc_prefix(std::fs::canonicalize(parent)?);
            if parent_after_digest != canonical_parent {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!("{} changed before atomic replacement", parent.display()),
                ));
            }
        }
        temp_file.persist(target).map_err(|error| error.error)?;
        sync_parent_dir(target)
    }
}

/// Containment for the atomic-replacement parent against the canonical
/// workspace roots. Each root is canonicalized here so a caller passing a
/// raw cwd (symlinked `/var` → `/private/var` on macOS) still matches.
fn atomic_parent_within_roots(canonical_parent: &Path, allowed_roots: &[PathBuf]) -> bool {
    allowed_roots.iter().any(|root| {
        let canonical_root =
            std::fs::canonicalize(root).map_or_else(|_| root.clone(), strip_unc_prefix);
        canonical_parent.starts_with(&canonical_root)
    })
}

fn atomic_replace_file(
    target: &Path,
    allowed_roots: &[PathBuf],
    contents: &[u8],
) -> std::io::Result<()> {
    atomic_replace_file_with(target, allowed_roots, contents, None, || {})
}

fn atomic_replace_file_if_unchanged(
    target: &Path,
    allowed_roots: &[PathBuf],
    contents: &[u8],
    expected_source: AtomicContentExpectation,
) -> std::io::Result<()> {
    atomic_replace_file_with(
        target,
        allowed_roots,
        contents,
        Some(expected_source),
        || {},
    )
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
    workspace: &crate::workspace::WorkspaceHandle,
) -> Result<ProcessedFiles> {
    // bd-cv653.3.12: the workspace handle is threaded through for
    // multi-root confinement; file args are read-only today (reads confine
    // through the tool layer), so the handle is accepted and unused here.
    let _ = workspace;
    let mut out = ProcessedFiles::default();

    for file_arg in file_args {
        let resolved = resolve_read_path(file_arg, cwd);
        let absolute_path = normalize_dot_segments(&resolved);
        let absolute_path = enforce_read_scope(&absolute_path, cwd, workspace)?;

        let meta = std::fs::metadata(&absolute_path).map_err(|e| {
            Error::tool(
                "read",
                format!("Cannot access file {}: {e}", absolute_path.display()),
            )
        })?;
        ensure_ancestors_searchable_sync(&absolute_path)
            .map_err(|err| Error::tool("read", err.to_string()))?;
        let required_access = if meta.is_dir() {
            UNIX_ACCESS_READ | UNIX_ACCESS_SEARCH
        } else {
            UNIX_ACCESS_READ
        };
        ensure_effective_mode_access(&meta, &absolute_path, required_access, "@file reading")
            .map_err(|err| Error::tool("read", err.to_string()))?;
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

        let allowed_roots = [cwd.to_path_buf(), Config::global_dir()];
        let bytes =
            read_file_capped_within_roots_sync(&absolute_path, &allowed_roots, READ_TOOL_MAX_BYTES)
                .map_err(|e| {
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

    // ubs:ignore false positive: image decode, not JWT processing.
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
    /// Discoverable-tier tool names (bd-cv653.1.6): excluded from the provider
    /// schema until promoted via `xdev promote`. Everything not in this set
    /// is in the schema (essential tier).
    discoverable: std::collections::HashSet<String>,
    /// Session undo recorder shared with write/edit/hashline_edit
    /// (bd-cv653.3.13); the interactive host reads it back for /undo //redo.
    mutation_recorder: Option<Arc<crate::undo::FileMutationRecorder>>,
}

impl ToolRegistry {
    /// Create a new registry with the specified tools enabled.
    pub fn new(enabled: &[&str], cwd: &Path, config: Option<&Config>) -> Self {
        Self::with_mutation_recorder(enabled, cwd, config, None, None)
    }

    /// The undo recorder attached at construction, if any.
    #[must_use]
    pub fn mutation_recorder(&self) -> Option<Arc<crate::undo::FileMutationRecorder>> {
        self.mutation_recorder.clone()
    }

    /// Like [`ToolRegistry::new`] but attaches a session undo recorder to the
    /// mutating file tools (bd-cv653.3.13). `workspace` installs the shared
    /// multi-root handle on every path-confining tool (bd-cv653.3.12).
    #[allow(clippy::too_many_lines)]
    pub fn with_mutation_recorder(
        enabled: &[&str],
        cwd: &Path,
        config: Option<&Config>,
        mutation_recorder: Option<Arc<crate::undo::FileMutationRecorder>>,
        workspace: Option<&WorkspaceHandle>,
    ) -> Self {
        let legacy_workspace = WorkspaceHandle::default();
        let workspace = workspace.unwrap_or(&legacy_workspace);
        let mut tools: Vec<Box<dyn Tool>> = Vec::new();
        let shell_path = config.and_then(|c| c.shell_path.clone());
        let shell_command_prefix = config.and_then(|c| c.shell_command_prefix.clone());
        let image_auto_resize = config.is_none_or(Config::image_auto_resize);
        let block_images = config
            .and_then(|c| c.images.as_ref().and_then(|i| i.block_images))
            .unwrap_or(false);

        for name in enabled {
            match *name {
                "read" => tools.push(Box::new(
                    ReadTool::with_settings(cwd, image_auto_resize, block_images)
                        .with_url_policy(
                            config
                                .and_then(|c| c.read.as_ref())
                                .and_then(|r| r.url_allow_private_targets)
                                .unwrap_or(false),
                        )
                        .with_workspace(workspace.clone()),
                )),
                "bash" => tools.push(Box::new(
                    BashTool::with_shell(cwd, shell_path.clone(), shell_command_prefix.clone())
                        .with_mediation(config.and_then(|c| c.bash.clone())),
                )),
                "edit" => tools.push(Box::new(
                    EditTool::new(cwd)
                        .with_mutation_recorder(mutation_recorder.clone())
                        .with_workspace(workspace.clone()),
                )),
                "write" => tools.push(Box::new(
                    WriteTool::new(cwd)
                        .with_mutation_recorder(mutation_recorder.clone())
                        .with_workspace(workspace.clone()),
                )),
                "grep" => tools.push(Box::new(
                    GrepTool::with_backend(cwd, search_backend_from_config(config))
                        .with_workspace(workspace.clone()),
                )),
                "find" => tools.push(Box::new(
                    FindTool::with_backend(cwd, search_backend_from_config(config))
                        .with_workspace(workspace.clone()),
                )),
                "ls" => tools.push(Box::new(LsTool::new(cwd).with_workspace(workspace.clone()))),
                "hashline_edit" => tools.push(Box::new(
                    HashlineEditTool::new(cwd)
                        .with_mutation_recorder(mutation_recorder.clone())
                        .with_workspace(workspace.clone()),
                )),
                "jobs" => tools.push(Box::new(JobsTool)),
                "hub" => tools.push(Box::new(HubTool::new(cwd))),
                "security_scan" => {
                    tools.push(Box::new(crate::security_scan::SecurityScanTool::new(cwd)));
                }
                "web_search" => tools.push(Box::new(crate::web_search::WebSearchTool::new())),
                "eval" => tools.push(Box::new(crate::eval::EvalTool::new(cwd))),
                "github" => tools.push(Box::new(crate::github::GithubTool::new(
                    cwd,
                    config.and_then(|c| c.gh_path.as_deref()),
                ))),
                // "ask" is session-host-coupled (its picker handler is
                // installed by the interactive host) and joins via
                // extend_tools in main.rs/sdk.rs, like "todo".
                "ast_grep" => tools.push(Box::new(crate::ast_tools::AstGrepTool::new(cwd))),
                "ast_edit" => tools.push(Box::new(crate::ast_tools::AstEditTool::new(cwd))),
                "lsp" => tools.push(Box::new(crate::lsp::LspTool::new(cwd, config))),
                "debug" => tools.push(Box::new(crate::debug::DebugTool::new(cwd, config))),
                "subagent" => {
                    let structured_results = config
                        .and_then(|c| c.subagent_structured_results)
                        .unwrap_or(false);
                    let role_model_spec = config.and_then(crate::app::subagent_role_spec);
                    tools.push(Box::new(
                        crate::subagents::SubagentTool::new(cwd)
                            .with_structured_results(structured_results)
                            .with_role_model_spec(role_model_spec),
                    ));
                }
                _ => {}
            }
        }

        // Memory bank (bd-cv653.4.1): retain/recall/reflect/memory_edit join
        // the active set only when `memory.backend: local`; off (default)
        // keeps them absent from the model's tool list entirely.
        if config.is_some_and(|cfg| cfg.memory_backend() == "local")
            && let Ok(store) = crate::memory::MemoryStore::open(cwd)
        {
            let store = std::sync::Arc::new(store);
            tools.push(Box::new(crate::memory::RetainTool::new(
                std::sync::Arc::clone(&store),
            )));
            tools.push(Box::new(crate::memory::RecallTool::new(
                std::sync::Arc::clone(&store),
            )));
            tools.push(Box::new(crate::memory::ReflectTool::new(
                std::sync::Arc::clone(&store),
            )));
            tools.push(Box::new(crate::memory::MemoryEditTool::new(
                std::sync::Arc::clone(&store),
            )));
            // learn captures lessons into the same bank (bd-cv653.4.2).
            tools.push(Box::new(LearnTool::new(store)));
        }

        // manage_skill (bd-cv653.4.2): CRUD over the isolated managed-skills
        // dir; always available — it can never touch user-authored skills.
        tools.push(Box::new(ManageSkillTool));

        // Tool load modes (bd-cv653.1.6): register the xdev dispatcher when
        // the enabled set contains discoverable-tier tools and `xdev` itself
        // is not turned off. The dispatcher's snapshot powers list/describe;
        // run/promote are intercepted by the agent executor.
        let discoverable_names;
        {
            let discoverable: Vec<crate::xdev::DiscoverableToolInfo> = tools
                .iter()
                .filter(|tool| {
                    crate::xdev::tier_for(tool.name(), config)
                        == crate::xdev::LoadMode::Discoverable
                })
                .map(|tool| crate::xdev::DiscoverableToolInfo {
                    name: tool.name().to_string(),
                    one_liner: crate::xdev::one_liner(tool.description()),
                    description: tool.description().to_string(),
                    parameters: tool.parameters(),
                })
                .collect();
            discoverable_names = discoverable.iter().map(|info| info.name.clone()).collect();
            let xdev_enabled = crate::xdev::tier_for("xdev", config) != crate::xdev::LoadMode::Off;
            if !discoverable.is_empty() && xdev_enabled {
                tools.push(Box::new(crate::xdev::XdevTool::new(cwd, discoverable)));
            }
        }

        Self {
            tools,
            discoverable: discoverable_names,
            mutation_recorder,
        }
    }

    /// Construct a registry from a pre-built tool list.
    pub fn from_tools(tools: Vec<Box<dyn Tool>>) -> Self {
        Self {
            tools,
            discoverable: std::collections::HashSet::new(),
            mutation_recorder: None,
        }
    }

    /// Convert the registry into the owned tool list.
    pub fn into_tools(self) -> Vec<Box<dyn Tool>> {
        self.tools
    }

    /// Whether a tool is currently discoverable-tier (hidden from the
    /// provider schema until promoted). bd-cv653.1.6.
    #[must_use]
    pub fn is_discoverable(&self, name: &str) -> bool {
        self.discoverable.contains(name)
    }

    /// Promote a discoverable tool into the live schema set.
    pub fn mark_promoted(&mut self, name: &str) {
        self.discoverable.remove(name);
    }

    /// Names + one-liners of currently discoverable tools (for the
    /// system-prompt index).
    #[must_use]
    pub fn discoverable_index(&self) -> Vec<(String, String)> {
        self.tools
            .iter()
            .filter(|tool| self.discoverable.contains(tool.name()))
            .map(|tool| {
                (
                    tool.name().to_string(),
                    crate::xdev::one_liner(tool.description()),
                )
            })
            .collect()
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
    artifact_root: Option<PathBuf>,
    /// SSRF override for URL reads (bd-cv653.2.2): allow private/loopback
    /// targets (from `read.urlAllowPrivateTargets`).
    allow_private_urls: bool,
    workspace: WorkspaceHandle,
    #[cfg(test)]
    after_open_hook: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl ReadTool {
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            workspace: WorkspaceHandle::default(),
            auto_resize: true,
            block_images: false,
            artifact_root: None,
            allow_private_urls: false,
            #[cfg(test)]
            after_open_hook: None,
        }
    }

    /// Attach the session workspace root handle (bd-cv653.3.12).
    #[must_use]
    pub fn with_workspace(mut self, workspace: WorkspaceHandle) -> Self {
        self.workspace = workspace;
        self
    }

    pub fn with_settings(cwd: &Path, auto_resize: bool, block_images: bool) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            workspace: WorkspaceHandle::default(),
            auto_resize,
            block_images,
            artifact_root: None,
            allow_private_urls: false,
            #[cfg(test)]
            after_open_hook: None,
        }
    }

    /// SSRF override for URL reads (bd-cv653.2.2).
    #[must_use]
    pub const fn with_url_policy(mut self, allow_private_urls: bool) -> Self {
        self.allow_private_urls = allow_private_urls;
        self
    }

    #[cfg(test)]
    fn with_artifact_root(cwd: &Path, artifact_root: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            workspace: WorkspaceHandle::default(),
            auto_resize: true,
            block_images: false,
            artifact_root: Some(artifact_root.to_path_buf()),
            allow_private_urls: false,
            after_open_hook: None,
        }
    }

    #[cfg(test)]
    fn with_after_open_hook(
        cwd: &Path,
        after_open_hook: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            workspace: WorkspaceHandle::default(),
            auto_resize: true,
            block_images: false,
            artifact_root: None,
            allow_private_urls: false,
            after_open_hook: Some(Arc::new(after_open_hook)),
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

impl ReadTool {
    /// Internal-scheme reads (bd-cv653.6.3): resolve via the URL router,
    /// then apply the same line-window/truncation contract as text files.
    async fn execute_scheme_read(&self, input: &ReadInput) -> Result<ToolOutput> {
        use std::fmt::Write as _;

        let resolved = crate::url_router::resolve(&input.path, &self.cwd)?;
        let total_lines = resolved.content.lines().count();
        let start_line = input
            .offset
            .and_then(|n| usize::try_from(n).ok())
            .map_or(0, |n| n.saturating_sub(1));
        if total_lines == 0 {
            return Ok(ToolOutput {
                content: vec![ContentBlock::Text(TextContent::new(""))],
                details: Some(serde_json::to_value(&resolved)?),
                is_error: false,
            });
        }
        if start_line >= total_lines {
            let offset_display = input.offset.unwrap_or(0);
            return Err(Error::tool(
                "read",
                format!(
                    "Offset {offset_display} is beyond end of document ({total_lines} lines total)"
                ),
            ));
        }
        let max_lines_for_truncation = input
            .limit
            .and_then(|l| usize::try_from(l).ok())
            .unwrap_or(DEFAULT_MAX_LINES);
        let limit_lines = input
            .limit
            .and_then(|l| usize::try_from(l).ok())
            .unwrap_or(usize::MAX);
        let lines_to_take = limit_lines.min(max_lines_for_truncation.saturating_add(1));

        let mut selected_content = String::new();
        let max_line_num = start_line.saturating_add(lines_to_take).min(total_lines);
        let line_num_width = max_line_num.to_string().len().max(5);
        for (i, line) in resolved.content.lines().skip(start_line).enumerate() {
            if i >= lines_to_take || start_line + i >= total_lines {
                break;
            }
            if i > 0 {
                selected_content.push('\n');
            }
            let line = line.strip_suffix('\r').unwrap_or(line);
            let line_idx = start_line + i;
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
        if truncation.truncated {
            let end_line = (start_line + lines_to_take).min(total_lines);
            let _ = write!(
                output_text,
                "\n\n[Showing lines {}-{end_line} of {total_lines}. Document: {}]",
                start_line + 1,
                input.path
            );
        }
        let mut details = serde_json::to_value(&resolved)?;
        details["truncated"] = serde_json::Value::Bool(truncation.truncated);
        details["totalLines"] = serde_json::Value::from(total_lines);
        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(output_text))],
            details: Some(details),
            is_error: false,
        })
    }

    /// URL reads (bd-cv653.2.2): fetch + convert, then window with the same
    /// 1-based offset/limit and continuation-notice shapes as file reads.
    /// 1-based offset/limit and continuation-notice shapes as file reads.
    /// A trailing `:raw` selector bypasses conversion (plaintext passthrough).
    async fn execute_url_read(&self, input: &ReadInput) -> Result<ToolOutput> {
        let (raw, url) = input.path.strip_suffix(":raw").map_or_else(
            || (false, input.path.clone()),
            |stripped| (true, stripped.to_string()),
        );
        let policy = if self.allow_private_urls {
            crate::url_read::SsrfPolicy::AllowPrivateTargets
        } else {
            crate::url_read::SsrfPolicy::BlockPrivateTargets
        };
        let mut outcome = crate::url_read::fetch_and_convert(&url, policy).await?;
        if raw {
            outcome.content = format!(
                "[raw fetch of {url}]\n\n{}",
                outcome.content // converters already ran; raw shows the wire text when it was plaintext
            );
        }
        if outcome.download_truncated {
            outcome.content.push_str(
                "\n\n[Download truncated at 10 MiB; the source page continues beyond this point.]",
            );
        }

        let lines: Vec<&str> = outcome.content.lines().collect();
        let total_lines = lines.len();
        let start_line = usize::try_from(input.offset.unwrap_or(1))
            .unwrap_or(1)
            .max(1);
        if start_line > total_lines && total_lines > 0 {
            return Err(Error::validation(format!(
                "Offset {start_line} is beyond end of URL content ({total_lines} lines total)"
            )));
        }
        let limit = usize::try_from(input.limit.unwrap_or(2000))
            .unwrap_or(2000)
            .max(1);
        let end_line = start_line
            .saturating_add(limit)
            .saturating_sub(1)
            .min(total_lines);
        let window = if total_lines == 0 {
            String::new()
        } else {
            lines[(start_line - 1)..end_line].join("\n")
        };

        let mut output_text = window;
        if end_line < total_lines {
            let next_offset = end_line + 1;
            let _ = std::fmt::Write::write_fmt(
                &mut output_text,
                format_args!(
                    "\n\n[Showing lines {start_line}-{end_line} of {total_lines}. Use offset={next_offset} to continue.]"
                ),
            );
        }

        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(output_text))],
            details: Some(serde_json::json!({
                "url": outcome.final_url,
                "contentType": outcome.kind.as_str(),
                "wireContentType": outcome.wire_content_type,
                "extractor": outcome.extractor,
                "totalLines": total_lines,
                "offset": start_line,
                "limit": limit,
                "truncated": end_line < total_lines || outcome.download_truncated,
            })),
            is_error: false,
        })
    }
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

    fn effects(&self) -> ToolEffects {
        ToolEffects::read()
    }

    #[allow(clippy::too_many_lines)]
    async fn execute(
        &self,
        tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let input_value = input.clone();
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

        // URL reads (bd-cv653.2.2): http(s) paths fetch + convert to
        // reader-mode markdown; pagination/truncation shapes match file reads.
        if input.path.starts_with("http://") || input.path.starts_with("https://") {
            return self.execute_url_read(&input).await;
        }

        // Internal URL schemes (bd-cv653.6.3): skill://, prompt://,
        // local://, conflict://, pr://, issue://, ssh:// resolve into
        // virtual documents with the same pagination contract as file
        // reads. Unknown schemes error with the registered list — never a
        // silent filesystem fallback.
        if crate::url_router::has_scheme(&input.path) {
            return self.execute_scheme_read(&input).await;
        }

        let path = resolve_read_path(&input.path, &self.cwd);
        let path = enforce_read_scope(&path, &self.cwd, &self.workspace)?;

        let path_for_open = path.clone();
        let mut allowed_roots = self.workspace.snapshot_or(&self.cwd).all();
        allowed_roots.push(Config::global_dir());
        #[cfg(test)]
        let after_open_hook = self.after_open_hook.clone();
        let (std_file, cache_file, meta, cache_deps) =
            asupersync::runtime::spawn_blocking_io(move || -> std::io::Result<_> {
                let file =
                    open_regular_file_within_roots_with(&path_for_open, &allowed_roots, || {})?;
                let metadata = file.metadata()?;
                let cache_file = file.try_clone().ok();
                #[cfg(test)]
                if let Some(after_open_hook) = after_open_hook {
                    after_open_hook();
                }
                let cache_deps = cache_file.as_ref().and_then(|cache_file| {
                    cache_dependency_for_open_file(&path_for_open, cache_file)
                });
                Ok((file, cache_file, metadata, cache_deps))
            })
            .await
            .map_err(|err| Error::tool("read", err.to_string()))?;

        let cache_key = tool_cache_key("read", &self.cwd, &input_value);
        if let Some(output) = cached_tool_output(&cache_key, cache_deps.as_deref()) {
            let stable_deps = stable_cache_dependency_for_open_file(
                &path,
                cache_file.as_ref(),
                cache_deps.as_deref(),
            );
            if stable_deps.is_some() {
                return Ok(output);
            }
        }

        let mut file = asupersync::fs::File::from_std(std_file);

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
            if resized.resized
                && let (Some(ow), Some(oh), Some(w), Some(h)) = (
                    resized.original_width,
                    resized.original_height,
                    resized.width,
                    resized.height,
                )
            {
                if w > 0 {
                    let scale = f64::from(ow) / f64::from(w);
                    let _ = write!(
                        note,
                        "\n[Image: original {ow}x{oh}, displayed at {w}x{h}. Multiply coordinates by {scale:.2} to map to original image.]"
                    );
                } else {
                    let _ = write!(note, "\n[Image: original {ow}x{oh}, displayed at {w}x{h}.]");
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
            let output = ToolOutput {
                content: vec![ContentBlock::Text(TextContent::new(""))],
                details: None,
                is_error: false,
            };
            cache_tool_output(
                cache_key,
                stable_cache_dependency_for_open_file(
                    &path,
                    cache_file.as_ref(),
                    cache_deps.as_deref(),
                ),
                &output,
            );
            return Ok(output);
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

        let artifact_source = (selected_content.len() > TOOL_OUTPUT_ARTIFACT_THRESHOLD_BYTES)
            .then(|| selected_content.clone());

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

        if let Some(artifact_source) = artifact_source.as_deref() {
            attach_text_artifact_if_needed_with_root(
                self.artifact_root.as_deref(),
                &mut output_text,
                &mut details,
                "read",
                tool_call_id,
                "selectedTextWindow",
                artifact_source,
            );
        }

        let output = ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(output_text))],
            details,
            is_error: false,
        };
        cache_tool_output(
            cache_key,
            stable_cache_dependency_for_open_file(
                &path,
                cache_file.as_ref(),
                cache_deps.as_deref(),
            ),
            &output,
        );
        Ok(output)
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
    /// Run detached as a background job (bd-cv653.3.10): returns a job id
    /// immediately; completion arrives as a follow-up message.
    background: Option<bool>,
}

pub struct BashTool {
    cwd: PathBuf,
    shell_path: Option<String>,
    command_prefix: Option<String>,
    artifact_root: Option<PathBuf>,
    /// Mediation settings (`bash.mediation*`, bd-cv653.1.7).
    mediation: Option<crate::config::BashSettings>,
}

#[derive(Debug, Clone)]
pub struct BashRunResult {
    pub output: String,
    pub exit_code: i32,
    pub cancelled: bool,
    pub cancellation_reason: Option<BashCancellationReason>,
    pub timeout_ms: Option<u64>,
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

fn bash_cancellation_details(
    reason: BashCancellationReason,
    timeout_ms: Option<u64>,
    exit_code: i32,
) -> serde_json::Value {
    serde_json::json!({
        "schema": BASH_CANCELLATION_SCHEMA_V1,
        "status": "cancelled",
        "reason": reason.as_str(),
        "cleanup": "process_group_tree_terminated",
        "exitCode": exit_code,
        "timeoutMs": timeout_ms,
    })
}

/// Dispatch to the pipe or PTY spawn path (bd-cv653.1.7) with an identical
/// call shape, so the mediation gate's arms don't duplicate the branch.
async fn execute_bash_spawn(
    cwd: &Path,
    shell_path: Option<&str>,
    command_prefix: Option<&str>,
    command: &str,
    timeout_secs: Option<u64>,
    on_update: Option<&(dyn Fn(ToolUpdate) + Send + Sync)>,
    use_pty: bool,
) -> Result<BashRunResult> {
    if use_pty {
        run_bash_command_pty(
            cwd,
            shell_path,
            command_prefix,
            command,
            timeout_secs,
            on_update,
        )
        .await
    } else {
        run_bash_command(
            cwd,
            shell_path,
            command_prefix,
            command,
            timeout_secs,
            on_update,
        )
        .await
    }
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
    attach_child_job_discipline(&child);

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
    let mut cancellation_reason: Option<BashCancellationReason> = None;
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
                cancellation_reason = Some(BashCancellationReason::Timeout);
                let pid = guard.child.as_ref().map(std::process::Child::id);
                terminate_process_group_tree(pid);
                terminate_deadline = Some(now + Duration::from_secs(BASH_TERMINATE_GRACE_SECS));
            }
        }

        if terminate_deadline.is_none() && cx.checkpoint().is_err() {
            cancelled = true;
            cancellation_reason = Some(BashCancellationReason::AmbientCancellation);
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
                cancellation_reason.get_or_insert(BashCancellationReason::AmbientCancellation);
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
    if guard.child.is_some()
        && let Ok(status) = guard.wait()
    {
        exit_code.get_or_insert_with(|| exit_status_code(status));
    }

    Ok(assemble_bash_run_result(
        bash_output,
        exit_code,
        timed_out,
        cancelled,
        cancellation_reason,
        timeout_secs,
    ))
}

/// Shared bash result assembly for the pipe and PTY spawn paths
/// (bd-cv653.1.7): truncation annotation, timeout suffix, and the
/// `BashRunResult` shape are identical across both.
fn assemble_bash_run_result(
    mut bash_output: BashOutputState,
    exit_code: Option<i32>,
    timed_out: bool,
    mut cancelled: bool,
    cancellation_reason: Option<BashCancellationReason>,
    timeout_secs: Option<u64>,
) -> BashRunResult {
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

    BashRunResult {
        output: output_text,
        exit_code,
        cancelled,
        cancellation_reason,
        timeout_ms: timeout_secs.map(|s| s.saturating_mul(1000)),
        truncated: truncation.truncated,
        full_output_path,
        truncation: if truncation.truncated {
            Some(truncation)
        } else {
            None
        },
    }
}

/// Run a bash command under a pseudo-terminal (bd-cv653.1.7). Selected for
/// isatty-requiring commands (`ssh`, `top`, `vim`, `python -i`, …) when
/// `bash.pty` is `auto`/`always`. The child sees a real controlling terminal,
/// so programs that probe `isatty(3)` behave exactly as they do in an
/// interactive shell. The observable tool contract — output truncation,
/// timeout/tree-kill, cancellation, streaming updates, `BashRunResult` — is
/// identical to the pipe path.
#[allow(clippy::too_many_lines)]
pub(crate) async fn run_bash_command_pty(
    cwd: &Path,
    shell_path: Option<&str>,
    command_prefix: Option<&str>,
    command: &str,
    timeout_secs: Option<u64>,
    on_update: Option<&(dyn Fn(ToolUpdate) + Send + Sync)>,
) -> Result<BashRunResult> {
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};

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

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 40,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| Error::tool("bash", format!("Failed to allocate PTY: {e}")))?;

    let mut pty_cmd = CommandBuilder::new(shell);
    pty_cmd.arg("-c");
    pty_cmd.arg(&command);
    pty_cmd.cwd(cwd);

    // portable-pty puts the child in its own session (setsid) on unix, so the
    // child pid is its process-group id and the shared tree-kill helpers work
    // exactly as they do for the pipe path's isolated process group.
    let mut child = pair
        .slave
        .spawn_command(pty_cmd)
        .map_err(|e| Error::tool("bash", format!("Failed to spawn shell on PTY: {e}")))?;
    // Drop our handle to the slave side so the master sees EOF when the child
    // exits; otherwise the pump thread would block forever.
    drop(pair.slave);
    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| Error::tool("bash", format!("Failed to clone PTY reader: {e}")))?;

    let (tx, rx) = mpsc::sync_channel::<BashPipeFrame>(1024);
    let pty_thread = thread::spawn(move || pump_stream(reader, "pty", &tx));

    let max_chunks_bytes = DEFAULT_MAX_BYTES.saturating_mul(2);
    let mut bash_output = BashOutputState::new(max_chunks_bytes);
    bash_output.timeout_ms = timeout_secs.map(|s| s.saturating_mul(1000));

    let cx = AgentCx::for_current_or_request();
    let mut timed_out = false;
    let mut cancelled = false;
    let mut cancellation_reason: Option<BashCancellationReason> = None;
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
                let _ = child.kill();
                return Err(err);
            }
            updated = true;
        }

        if updated {
            emit_bash_update(&bash_output, on_update)?;
        }

        match child.try_wait() {
            Ok(Some(status)) => {
                exit_code = Some(i32::try_from(status.exit_code()).unwrap_or(-1));
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
                let _ = child.kill();
                if let Ok(status) = child.wait() {
                    exit_code = Some(i32::try_from(status.exit_code()).unwrap_or(-1));
                }
                break;
            }
        } else if let Some(timeout) = timeout {
            let elapsed = std::time::Duration::from_nanos(now.duration_since(start));
            if elapsed >= timeout {
                timed_out = true;
                cancellation_reason = Some(BashCancellationReason::Timeout);
                terminate_process_group_tree(child.process_id());
                terminate_deadline = Some(now + Duration::from_secs(BASH_TERMINATE_GRACE_SECS));
            }
        }

        if terminate_deadline.is_none() && cx.checkpoint().is_err() {
            cancelled = true;
            cancellation_reason = Some(BashCancellationReason::AmbientCancellation);
            let _ = child.kill();
            exit_code = Some(-1);
            break;
        }

        sleep(now, tick).await;
    }

    // Drain remaining frames while the pump observes EOF (same 5s safety cap
    // as the pipe path for fd-inheriting grandchildren).
    {
        let drain_start = cx
            .cx()
            .timer_driver()
            .map_or_else(wall_now, |timer| timer.now());
        let drain_deadline = drain_start + Duration::from_secs(5);
        loop {
            let mut got_data = false;
            while let Ok(frame) = rx.try_recv() {
                if let Err(err) = ingest_bash_pipe_frame(frame, &mut bash_output).await {
                    let _ = child.kill();
                    return Err(err);
                }
                got_data = true;
            }
            if got_data {
                emit_bash_update(&bash_output, on_update)?;
            }

            if pty_thread.is_finished() {
                while let Ok(frame) = rx.try_recv() {
                    if let Err(err) = ingest_bash_pipe_frame(frame, &mut bash_output).await {
                        let _ = child.kill();
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
            sleep(now, tick).await;
        }
    }

    // Reap the child to avoid zombies (belt-and-suspenders with try_wait).
    if let Ok(status) = child.wait() {
        exit_code.get_or_insert_with(|| i32::try_from(status.exit_code()).unwrap_or(-1));
    }

    Ok(assemble_bash_run_result(
        bash_output,
        exit_code,
        timed_out,
        cancelled,
        cancellation_reason,
        timeout_secs,
    ))
}

impl BashTool {
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            shell_path: None,
            command_prefix: None,
            artifact_root: None,
            mediation: None,
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
            artifact_root: None,
            mediation: None,
        }
    }

    /// Attach mediation settings (`bash.mediation*`, bd-cv653.1.7).
    #[must_use]
    pub fn with_mediation(mut self, mediation: Option<crate::config::BashSettings>) -> Self {
        self.mediation = mediation;
        self
    }

    #[cfg(test)]
    fn with_artifact_root(cwd: &Path, artifact_root: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            shell_path: None,
            command_prefix: None,
            artifact_root: Some(artifact_root.to_path_buf()),
            mediation: None,
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
                },
                "background": {
                    "type": "boolean",
                    "description": "Run detached as a background job: returns a job id immediately; output streams to an artifact file and a completion notice arrives as a follow-up message. Manage with the jobs tool (list/wait/cancel)."
                }
            },
            "required": ["command"]
        })
    }

    fn effects(&self) -> ToolEffects {
        ToolEffects::process().union(ToolEffects::write())
    }

    #[allow(clippy::too_many_lines)]
    async fn execute(
        &self,
        tool_call_id: &str,
        input: serde_json::Value,
        on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let input: BashInput =
            serde_json::from_value(input).map_err(|e| Error::validation(e.to_string()))?;

        // PTY auto-selection (bd-cv653.1.7): isatty-requiring commands get a
        // real pseudo-terminal when `bash.pty` is auto/always; the tool
        // contract is identical either way.
        let use_pty = match crate::bash_mediation::PtyMode::from_setting(
            self.mediation.as_ref().and_then(|s| s.pty.as_deref()),
        ) {
            crate::bash_mediation::PtyMode::Off => false,
            crate::bash_mediation::PtyMode::Always => true,
            crate::bash_mediation::PtyMode::Auto => {
                crate::bash_mediation::pty_required(&input.command)
            }
        };

        // Mediation gate (bd-cv653.1.7): classify before spawn against
        // `bash.mediation`. `forced` beats any approval override; off mode
        // is byte-identical to the pre-mediation path.
        if let Some(mediation) = &self.mediation {
            let mode =
                crate::bash_mediation::MediationMode::from_setting(mediation.mediation.as_deref());
            if mode != crate::bash_mediation::MediationMode::Off {
                // DCG policy bridge: imported allow_patterns from .dcg.toml
                // (project + global) win over every classifier hit — users
                // maintain ONE rule set.
                let allows = crate::bash_mediation::import_dcg_overrides(
                    &self.cwd,
                    &crate::config::Config::global_dir(),
                );
                if crate::bash_mediation::covered_by_allow(&input.command, &allows) {
                    let verdict = crate::bash_mediation::MediationVerdict::Allow { hits: vec![] };
                    let _ = verdict;
                } else {
                    let verdict =
                        crate::bash_mediation::assess(&input.command, mediation, mode, &self.cwd);
                    match verdict {
                        crate::bash_mediation::MediationVerdict::Allow { hits } => {
                            if !hits.is_empty() {
                                let payload =
                                    crate::bash_mediation::MediationVerdict::Allow { hits }
                                        .audit_payload(mode, &input.command);
                                tracing::info!(
                                    event = "pi.bash.mediation",
                                    payload = %payload,
                                    "bash mediation allow with hits"
                                );
                            }
                        }
                        crate::bash_mediation::MediationVerdict::Warn { hits } => {
                            let payload = crate::bash_mediation::MediationVerdict::Warn { hits }
                                .audit_payload(mode, &input.command);
                            tracing::info!(
                                event = "pi.bash.mediation",
                                payload = %payload,
                                "bash mediation warn"
                            );
                            let rules = payload["hits"]
                                .as_array()
                                .map(|hits| {
                                    hits.iter()
                                        .filter_map(|hit| hit["ruleId"].as_str())
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                })
                                .unwrap_or_default();
                            if input.background.unwrap_or(false) {
                                // Warn + background: annotate the job-start
                                // result instead of blocking on foreground
                                // output.
                                let job = crate::jobs::spawn_background(
                                    &self.cwd,
                                    self.shell_path.as_deref(),
                                    self.command_prefix.as_deref(),
                                    &input.command,
                                    input.timeout,
                                    self.artifact_root.as_deref(),
                                )?;
                                let mut details = serde_json::to_value(&job)?;
                                details["mediation"] = payload;
                                return Ok(ToolOutput {
                                    content: vec![ContentBlock::Text(TextContent::new(format!(
                                        "[MEDIATION WARN: {rules}]\n\nBackground job {} started \
                                         (pid {}). Output streams to {}. A completion notice \
                                         will arrive as a follow-up message.",
                                        job.id,
                                        job.pid.map_or_else(
                                            || "unknown".to_string(),
                                            |pid| pid.to_string()
                                        ),
                                        job.artifact_path
                                    )))],
                                    details: Some(details),
                                    is_error: false,
                                });
                            }
                            let mut result = execute_bash_spawn(
                                &self.cwd,
                                self.shell_path.as_deref(),
                                self.command_prefix.as_deref(),
                                &input.command,
                                input.timeout,
                                on_update.as_deref(),
                                use_pty,
                            )
                            .await?;
                            result.output =
                                format!("[MEDIATION WARN: {rules}]\n\n{}", result.output);
                            return Ok(ToolOutput {
                                content: vec![ContentBlock::Text(TextContent::new(result.output))],
                                details: Some(payload),
                                is_error: false,
                            });
                        }
                        crate::bash_mediation::MediationVerdict::Block { hits } => {
                            let payload = crate::bash_mediation::MediationVerdict::Block { hits }
                                .audit_payload(mode, &input.command);
                            tracing::info!(
                                event = "pi.bash.mediation",
                                payload = %payload,
                                "bash mediation block"
                            );
                            let reasons = payload["hits"]
                                .as_array()
                                .map(|hits| {
                                    hits.iter()
                                        .map(|hit| {
                                            format!(
                                                "- {} [{}]: {}",
                                                hit["ruleId"].as_str().unwrap_or("?"),
                                                hit["tier"].as_str().unwrap_or("?"),
                                                hit["reason"].as_str().unwrap_or("?"),
                                            )
                                        })
                                        .collect::<Vec<_>>()
                                        .join("\n")
                                })
                                .unwrap_or_default();
                            return Ok(ToolOutput {
                                content: vec![ContentBlock::Text(TextContent::new(format!(
                                    "[MEDIATION BLOCK] command refused by bash.mediation mode '{}':\n{reasons}\n\nCommand: {}",
                                    mode.as_str(),
                                    input.command
                                )))],
                                details: Some(payload),
                                is_error: true,
                            });
                        }
                    }
                }
            }
        }

        // Background dispatch (bd-cv653.3.10): the mediation gate above has
        // already classified the command; detached jobs run under the same
        // timeout/tree-kill discipline via the jobs registry.
        if input.background.unwrap_or(false) {
            let job = match crate::jobs::spawn_background(
                &self.cwd,
                self.shell_path.as_deref(),
                self.command_prefix.as_deref(),
                &input.command,
                input.timeout,
                self.artifact_root.as_deref(),
            ) {
                Ok(job) => job,
                Err(err) => {
                    // Named refusals (PI_JOBS_AT_CAPACITY, …) surface as a
                    // tool result the model can read, not a transport error.
                    return Ok(ToolOutput {
                        content: vec![ContentBlock::Text(TextContent::new(err.to_string()))],
                        details: None,
                        is_error: true,
                    });
                }
            };
            let details = serde_json::to_value(&job)?;
            return Ok(ToolOutput {
                content: vec![ContentBlock::Text(TextContent::new(format!(
                    "Background job {} started (pid {}). Output streams to {}. \
                     A completion notice will arrive as a follow-up message; \
                     manage it with the jobs tool (list/wait/cancel).",
                    job.id,
                    job.pid
                        .map_or_else(|| "unknown".to_string(), |pid| pid.to_string()),
                    job.artifact_path
                )))],
                details: Some(details),
                is_error: false,
            });
        }

        let result = execute_bash_spawn(
            &self.cwd,
            self.shell_path.as_deref(),
            self.command_prefix.as_deref(),
            &input.command,
            input.timeout,
            on_update.as_deref(),
            use_pty,
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
        if let Some(reason) = result.cancellation_reason {
            details_map.insert(
                "cancellation".to_string(),
                bash_cancellation_details(reason, result.timeout_ms, result.exit_code),
            );
        }

        let details = if details_map.is_empty() {
            None
        } else {
            Some(serde_json::Value::Object(details_map))
        };
        let mut details = details;
        let mut output_text = result.output;

        if let Some(path) = result.full_output_path.as_deref() {
            attach_text_artifact_from_path_if_needed_with_root(
                self.artifact_root.as_deref(),
                &mut output_text,
                &mut details,
                "bash",
                tool_call_id,
                "fullCommandOutput",
                Path::new(path),
            );
        }

        let is_error = result.cancelled || result.exit_code != 0;

        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(output_text))],
            details,
            is_error,
        })
    }
}

// ============================================================================
// Jobs Tool (bd-cv653.3.10)
// ============================================================================

/// Input parameters for the jobs tool.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JobsInput {
    /// `list` (all jobs), `wait` (block until settle or budget), or
    /// `cancel` (TERM → grace → KILL + tree walk).
    action: String,
    /// Job id for `wait`/`cancel` (from the bash background start result).
    job_id: Option<String>,
    /// Wait budget in milliseconds for `wait` (default 30000, max 600000).
    timeout_ms: Option<u64>,
}

/// Manage background bash jobs spawned with `bash {background: true}`
/// (bd-cv653.3.10). The future hub tool's jobs action group wraps the same
/// registry (bd-cv653.5.4).
pub struct JobsTool;

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Tool for JobsTool {
    fn name(&self) -> &str {
        "jobs"
    }

    fn label(&self) -> &str {
        "jobs"
    }

    fn description(&self) -> &str {
        "Manage background bash jobs started with `bash {background: true}`. \
         Actions: `list` (every job with status/pid/artifact), `wait` (block \
         until the job settles, bounded), `cancel` (kill the whole process \
         tree). Completion notices also arrive automatically as follow-up \
         messages."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list", "wait", "cancel"],
                    "description": "list | wait | cancel"
                },
                "jobId": {
                    "type": "string",
                    "description": "Job id (required for wait and cancel)"
                },
                "timeoutMs": {
                    "type": "integer",
                    "description": "Wait budget in milliseconds for wait (default 30000, max 600000)"
                }
            },
            "required": ["action"]
        })
    }

    fn effects(&self) -> ToolEffects {
        ToolEffects::process()
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let input: JobsInput =
            serde_json::from_value(input).map_err(|e| Error::validation(e.to_string()))?;
        let action = input.action.trim().to_ascii_lowercase();
        let payload = match action.as_str() {
            "list" => {
                let jobs = crate::jobs::list()?;
                serde_json::json!({ "schema": crate::jobs::JOB_SCHEMA, "jobs": jobs })
            }
            "wait" => {
                let job_id = input
                    .job_id
                    .as_deref()
                    .ok_or_else(|| Error::validation("jobs wait requires jobId".to_string()))?;
                let budget_ms = input.timeout_ms.unwrap_or(30_000).min(600_000);
                let snapshot =
                    crate::jobs::wait(job_id, std::time::Duration::from_millis(budget_ms))?;
                serde_json::to_value(&snapshot)?
            }
            "cancel" => {
                let job_id = input
                    .job_id
                    .as_deref()
                    .ok_or_else(|| Error::validation("jobs cancel requires jobId".to_string()))?;
                let snapshot = crate::jobs::cancel(job_id)?;
                serde_json::to_value(&snapshot)?
            }
            other => {
                return Err(Error::validation(format!(
                    "Unknown jobs action '{other}'; expected list, wait, or cancel"
                )));
            }
        };
        let text = match action.as_str() {
            "list" => {
                let count = payload["jobs"].as_array().map_or(0, Vec::len);
                if count == 0 {
                    "No background jobs this session.".to_string()
                } else {
                    let lines: Vec<String> = payload["jobs"]
                        .as_array()
                        .map(|jobs| {
                            jobs.iter()
                                .map(|job| {
                                    format!(
                                        "{}: {} (pid {}, exit {}, artifact {})",
                                        job["id"].as_str().unwrap_or("?"),
                                        job["status"].as_str().unwrap_or("?"),
                                        job["pid"].as_i64().map_or_else(
                                            || "n/a".to_string(),
                                            |pid| pid.to_string()
                                        ),
                                        job["exitCode"].as_i64().map_or_else(
                                            || "n/a".to_string(),
                                            |code| code.to_string()
                                        ),
                                        job["artifactPath"].as_str().unwrap_or("?")
                                    )
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    format!("{count} background job(s):\n{}", lines.join("\n"))
                }
            }
            "wait" | "cancel" => format!(
                "job {}: {} (exit {})\noutput tail:\n{}",
                payload["id"].as_str().unwrap_or("?"),
                payload["status"].as_str().unwrap_or("?"),
                payload["exitCode"]
                    .as_i64()
                    .map_or_else(|| "n/a".to_string(), |code| code.to_string()),
                payload["outputTail"].as_str().unwrap_or("")
            ),
            _ => String::new(),
        };
        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(text))],
            details: Some(payload),
            is_error: false,
        })
    }
}

// ============================================================================
// Learn Tool (bd-cv653.4.2)
// ============================================================================

/// Input parameters for the learn tool.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LearnInput {
    /// The reusable lesson to capture.
    lesson: String,
    /// Optional context (when/why this applies).
    context: Option<String>,
    /// Also draft a managed skill from the lesson (default false).
    promote: Option<bool>,
    /// Optional skill name for promotion (slugified otherwise).
    skill_name: Option<String>,
}

/// Capture a reusable lesson (memory kind=lesson, bd-cv653.4.1) and
/// optionally promote it into a managed skill.
pub struct LearnTool {
    store: std::sync::Arc<crate::memory::MemoryStore>,
}

impl LearnTool {
    pub const fn new(store: std::sync::Arc<crate::memory::MemoryStore>) -> Self {
        Self { store }
    }
}

/// Derive a valid skill slug from free text: lowercase, non-alnum → hyphen,
/// collapsed, trimmed, capped at 48 chars.
fn slugify(text: &str) -> String {
    let mut slug = String::with_capacity(text.len().min(48));
    let mut last_was_hyphen = true; // suppress leading hyphen
    for ch in text.chars() {
        let mapped = if ch.is_ascii_alphanumeric() {
            Some(ch.to_ascii_lowercase())
        } else {
            None
        };
        match mapped {
            Some(c) => {
                slug.push(c);
                last_was_hyphen = false;
            }
            None if !last_was_hyphen => {
                slug.push('-');
                last_was_hyphen = true;
            }
            None => {}
        }
        if slug.len() >= 48 {
            break;
        }
    }
    let slug = slug.trim_end_matches('-').to_string();
    if slug.is_empty() {
        "lesson".to_string()
    } else {
        slug
    }
}

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Tool for LearnTool {
    fn name(&self) -> &str {
        "learn"
    }

    fn label(&self) -> &str {
        "learn"
    }

    fn description(&self) -> &str {
        "Capture a reusable lesson into this project's memory bank (survives \
         sessions). With promote=true, also draft a managed skill from the \
         lesson (lint-gated; invalid drafts stay lessons with a warning)."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "lesson": { "type": "string", "description": "The reusable lesson" },
                "context": { "type": "string", "description": "When/why this applies (optional)" },
                "promote": { "type": "boolean", "description": "Also draft a managed skill (default false)" },
                "skillName": { "type": "string", "description": "Skill name for promotion (derived from the lesson otherwise)" }
            },
            "required": ["lesson"]
        })
    }

    fn effects(&self) -> ToolEffects {
        ToolEffects::write()
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let input: LearnInput =
            serde_json::from_value(input).map_err(|e| Error::validation(e.to_string()))?;
        if input.lesson.trim().is_empty() {
            return Err(Error::validation(
                "learn requires a non-empty lesson".to_string(),
            ));
        }
        let content = input.context.as_deref().map_or_else(
            || input.lesson.clone(),
            |ctx| format!("{}\n\nContext: {ctx}", input.lesson),
        );
        let memory = self.store.retain(
            crate::memory::MemoryKind::Lesson,
            &content,
            &["learn".to_string()],
            None,
        )?;
        let mut lines = vec![format!("Lesson captured [{}].", memory.id)];
        let mut promoted: Option<serde_json::Value> = None;
        if input.promote.unwrap_or(false) {
            let name = input
                .skill_name
                .as_deref()
                .map(slugify)
                .filter(|slug| slug != "lesson")
                .unwrap_or_else(|| slugify(&input.lesson));
            let description: String = input.lesson.chars().take(200).collect();
            match crate::skills_managed::create(&name, &description, &content) {
                Ok(info) => {
                    lines.push(format!(
                        "Promoted to managed skill '{name}' ({}).",
                        info.path
                    ));
                    promoted = Some(serde_json::to_value(&info)?);
                }
                Err(err) => {
                    lines.push(format!("Skill promotion skipped: {err} (lesson kept)."));
                }
            }
        }
        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(lines.join("\n")))],
            details: Some(serde_json::json!({
                "schema": crate::memory::MEMORY_SCHEMA,
                "memory": memory,
                "promoted": promoted,
            })),
            is_error: false,
        })
    }
}

// ============================================================================
// Manage Skill Tool (bd-cv653.4.2)
// ============================================================================

/// Input parameters for the manage_skill tool.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManageSkillInput {
    /// create | update | delete | list.
    op: String,
    /// Skill name (required for create/update/delete).
    name: Option<String>,
    /// Description (create; optional for update).
    description: Option<String>,
    /// Skill body markdown (create/update).
    content: Option<String>,
}

/// CRUD over the isolated managed-skills dir. The managed tier loads dead
/// last — user/project skills always win collisions.
pub struct ManageSkillTool;

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Tool for ManageSkillTool {
    fn name(&self) -> &str {
        "manage_skill"
    }

    fn label(&self) -> &str {
        "manage skill"
    }

    fn description(&self) -> &str {
        "Create, update, delete, or list agent-authored managed skills. \
         Managed skills load dead-last in precedence (user/project skills \
         always win) and carry a `managed: true` marker; operations on \
         content lacking the marker are refused."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "op": {
                    "type": "string",
                    "enum": ["create", "update", "delete", "list"],
                    "description": "Operation"
                },
                "name": { "type": "string", "description": "Skill name (create/update/delete)" },
                "description": { "type": "string", "description": "Skill description (create; optional for update)" },
                "content": { "type": "string", "description": "Skill body markdown (create/update)" }
            },
            "required": ["op"]
        })
    }

    fn effects(&self) -> ToolEffects {
        ToolEffects::write()
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let input: ManageSkillInput =
            serde_json::from_value(input).map_err(|e| Error::validation(e.to_string()))?;
        let op = input.op.trim().to_ascii_lowercase();
        let name_required = |op: &str| -> Result<String> {
            input
                .name
                .clone()
                .filter(|name| !name.trim().is_empty())
                .ok_or_else(|| Error::validation(format!("manage_skill {op} requires name")))
        };
        let result: Result<(String, serde_json::Value)> = (|| match op.as_str() {
            "create" => {
                let name = name_required("create")?;
                let description = input.description.clone().ok_or_else(|| {
                    Error::validation("manage_skill create requires description".to_string())
                })?;
                let content = input.content.clone().unwrap_or_default();
                let info = crate::skills_managed::create(&name, &description, &content)?;
                Ok((
                    format!("Managed skill '{name}' created at {}.", info.path),
                    serde_json::to_value(&info)?,
                ))
            }
            "update" => {
                let name = name_required("update")?;
                let content = input.content.clone().unwrap_or_default();
                let info =
                    crate::skills_managed::update(&name, input.description.as_deref(), &content)?;
                Ok((
                    format!("Managed skill '{name}' updated."),
                    serde_json::to_value(&info)?,
                ))
            }
            "delete" => {
                let name = name_required("delete")?;
                crate::skills_managed::delete(&name)?;
                Ok((
                    format!("Managed skill '{name}' deleted."),
                    serde_json::json!({
                        "schema": crate::skills_managed::SKILL_SCHEMA,
                        "name": name,
                        "op": "delete",
                    }),
                ))
            }
            "list" => {
                let skills = crate::skills_managed::list()?;
                let text = if skills.is_empty() {
                    "No managed skills.".to_string()
                } else {
                    skills
                        .iter()
                        .map(|skill| {
                            format!(
                                "- {} (managed: {}): {}\n  {}",
                                skill.name, skill.managed, skill.description, skill.path
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                Ok((
                    text,
                    serde_json::json!({
                        "schema": crate::skills_managed::SKILL_SCHEMA,
                        "skills": skills,
                    }),
                ))
            }
            other => Err(Error::validation(format!(
                "Unknown manage_skill op '{other}'; expected create, update, delete, or list"
            ))),
        })();
        match result {
            Ok((text, details)) => Ok(ToolOutput {
                content: vec![ContentBlock::Text(TextContent::new(text))],
                details: Some(details),
                is_error: false,
            }),
            Err(err) => Ok(ToolOutput {
                content: vec![ContentBlock::Text(TextContent::new(err.to_string()))],
                details: Some(serde_json::json!({ "error": err.to_string() })),
                is_error: true,
            }),
        }
    }
}

// ============================================================================
// Hub Tool (bd-cv653.5.4)
// ============================================================================

/// Input parameters for the hub tool (omp hub parity).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HubInput {
    /// `start` | `ps` | `logs` | `stop` | `restart` | `describe` | `send` |
    /// `jobs`.
    op: String,
    /// Service name (start/ps target). Unique per project directory.
    name: Option<String>,
    /// Program to spawn for `start` (omp parity: `application`).
    application: Option<String>,
    /// Program arguments for `start`.
    args: Option<Vec<String>>,
    /// Working directory for `start` (default: tool cwd).
    cwd: Option<String>,
    /// Extra environment for `start` (`{"KEY": "value"}`).
    env: Option<std::collections::HashMap<String, String>>,
    /// Readiness gates: log regex AND/OR TCP port, plus timeout seconds.
    ready: Option<HubReadyInput>,
    /// Survive session exit (state file re-discovery).
    detached: Option<bool>,
    /// `logs`: opaque cursor from a previous page (incremental read).
    cursor: Option<u64>,
    /// `logs`: last N lines (snapshot read).
    tail: Option<usize>,
    /// `logs`: substring filter.
    grep: Option<String>,
    /// `logs`: bounded wait for matching/new lines in milliseconds.
    wait_ms: Option<u64>,
    /// `send`: literal text to write to the service's PTY stdin.
    text: Option<String>,
    /// `send`: append ENTER after `text` (default true).
    enter: Option<bool>,
    /// `send`: named keys (ENTER, TAB, ESCAPE, CTRL_C, CTRL_D, arrows).
    keys: Option<Vec<String>>,
    /// `send`: signal name (SIGINT, SIGTERM, SIGHUP, SIGQUIT, SIGKILL).
    signal: Option<String>,
    /// `jobs`: list | wait | cancel.
    /// `agent`: roster | transcript | steer | kill | revive | send | inbox.
    action: Option<String>,
    /// `jobs`: job id for wait/cancel.
    job_id: Option<String>,
    /// `agent steer|send`: sender label recorded on the bus message.
    from: Option<String>,
    /// `jobs`: wait budget in milliseconds.
    timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HubReadyInput {
    log: Option<String>,
    port: Option<u16>,
    timeout_secs: Option<u64>,
}

/// Supervise long-running processes (bd-cv653.5.4) and manage background
/// jobs (bd-cv653.3.10) from one surface. The `messaging` action group
/// lands with the agent-hub registry (bd-cv653.5.3).
pub struct HubTool {
    cwd: PathBuf,
}

impl HubTool {
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
        }
    }
}

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Tool for HubTool {
    fn name(&self) -> &str {
        "hub"
    }

    fn label(&self) -> &str {
        "hub"
    }

    fn description(&self) -> &str {
        "Supervise long-running processes and manage background jobs. A dev \
         server, watcher, debugger, REPL, or any process needing later input \
         MUST use hub, not bash. Ops: `start` (spawn with optional readiness \
         gates: ready.log regex AND ready.port TCP accept, both must pass \
         within ready.timeoutSecs — start returns only after readiness is \
         observed), `ps` (list services), `logs` (tail/grep/incremental \
         cursor reads with bounded wait), `send` (PTY stdin: text, named \
         keys, signals), `stop` (graceful tree termination), `restart` \
         (retained launch spec), `describe` (full descriptor), `jobs` \
         (background bash jobs: list/wait/cancel), `agent` (subagent children: \
         roster/transcript/steer/kill/revive/send/inbox)."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "op": {
                    "type": "string",
                    "enum": ["start", "ps", "logs", "stop", "restart", "describe", "send", "jobs", "agent"],
                    "description": "Operation"
                },
                "name": { "type": "string", "description": "Service name (unique per project)" },
                "application": { "type": "string", "description": "Program to spawn (start)" },
                "args": { "type": "array", "items": { "type": "string" }, "description": "Program arguments (start)" },
                "cwd": { "type": "string", "description": "Working directory (start; default: session cwd)" },
                "env": { "type": "object", "description": "Extra environment (start)" },
                "ready": {
                    "type": "object",
                    "properties": {
                        "log": { "type": "string", "description": "Regex that must match service output" },
                        "port": { "type": "integer", "description": "TCP port on 127.0.0.1 that must accept" },
                        "timeoutSecs": { "type": "integer", "description": "Readiness budget (default 30)" }
                    },
                    "description": "Readiness gates; all supplied gates must pass"
                },
                "detached": { "type": "boolean", "description": "Survive session exit (default false)" },
                "cursor": { "type": "integer", "description": "logs: opaque cursor for incremental reads" },
                "tail": { "type": "integer", "description": "logs: last N lines" },
                "grep": { "type": "string", "description": "logs: substring filter" },
                "waitMs": { "type": "integer", "description": "logs: bounded wait in ms (max 60000)" },
                "text": { "type": "string", "description": "send: text for PTY stdin" },
                "enter": { "type": "boolean", "description": "send: append ENTER after text (default true)" },
                "keys": { "type": "array", "items": { "type": "string" }, "description": "send: named keys (ENTER, TAB, ESCAPE, CTRL_C, CTRL_D, UP, DOWN, LEFT, RIGHT)" },
                "signal": { "type": "string", "description": "send: SIGINT, SIGTERM, SIGHUP, SIGQUIT, or SIGKILL" },
                "action": { "type": "string", "enum": ["list", "wait", "cancel", "roster", "transcript", "steer", "kill", "revive", "send", "inbox"], "description": "jobs: list/wait/cancel; agent: roster/transcript/steer/kill/revive/send/inbox" },
                "from": { "type": "string", "description": "agent steer/send: sender label recorded on the bus message" },
                "jobId": { "type": "string", "description": "jobs: job id for wait/cancel" },
                "timeoutMs": { "type": "integer", "description": "jobs: wait budget in ms" }
            },
            "required": ["op"]
        })
    }

    fn effects(&self) -> ToolEffects {
        ToolEffects::process()
    }

    #[allow(clippy::too_many_lines)]
    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let input: HubInput =
            serde_json::from_value(input).map_err(|e| Error::validation(e.to_string()))?;
        let op = input.op.trim().to_ascii_lowercase();
        let dispatched = self.dispatch(&op, &input);
        let (text, details, is_error) = match dispatched {
            Ok((text, details)) => (text, details, false),
            Err(err) => {
                // Domain refusals (PI_HUB_*) surface as tool results the
                // model can read, matching the jobs capacity contract.
                (
                    err.to_string(),
                    serde_json::json!({ "error": err.to_string() }),
                    true,
                )
            }
        };
        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(text))],
            details: Some(details),
            is_error,
        })
    }
}

impl HubTool {
    #[allow(clippy::too_many_lines)]
    fn dispatch(&self, op: &str, input: &HubInput) -> Result<(String, serde_json::Value)> {
        let name_required = |op: &str| -> Result<String> {
            input
                .name
                .clone()
                .filter(|name| !name.trim().is_empty())
                .ok_or_else(|| Error::validation(format!("hub {op} requires name")))
        };
        let (text, details) = match op {
            "start" => {
                let name = name_required("start")?;
                let application = input
                    .application
                    .clone()
                    .filter(|app| !app.trim().is_empty())
                    .ok_or_else(|| {
                        Error::validation("hub start requires application".to_string())
                    })?;
                let cwd = input
                    .cwd
                    .as_deref()
                    .map_or_else(|| self.cwd.clone(), PathBuf::from);
                let ready = input.ready.as_ref().map(|ready| crate::hub::ReadySpec {
                    log: ready.log.clone(),
                    port: ready.port,
                    timeout_secs: ready.timeout_secs,
                });
                let spec = crate::hub::LaunchSpec {
                    name: name.clone(),
                    program: application,
                    args: input.args.clone().unwrap_or_default(),
                    cwd,
                    env: input.env.clone().unwrap_or_default().into_iter().collect(),
                    ready,
                    detached: input.detached.unwrap_or(false),
                };
                let snapshot = crate::hub::start(&spec)?;
                let details = serde_json::to_value(&snapshot)?;
                let text = format!(
                    "Service '{name}' is running (pid {}, log: {}).",
                    snapshot
                        .pid
                        .map_or_else(|| "?".to_string(), |pid| pid.to_string()),
                    snapshot.log_path
                );
                (text, details)
            }
            "ps" => {
                let services = crate::hub::ps()?;
                let details = serde_json::json!({
                    "schema": crate::hub::SERVICE_SCHEMA,
                    "services": services,
                });
                let text = if services.is_empty() {
                    "No supervised services this session.".to_string()
                } else {
                    let lines: Vec<String> = services
                        .iter()
                        .map(|svc| {
                            format!(
                                "{}: {} (pid {}, command `{}`, log {})",
                                svc.name,
                                svc.status,
                                svc.pid
                                    .map_or_else(|| "n/a".to_string(), |pid| pid.to_string()),
                                svc.command,
                                svc.log_path
                            )
                        })
                        .collect();
                    format!("{} service(s):\n{}", services.len(), lines.join("\n"))
                };
                (text, details)
            }
            "logs" => {
                let name = name_required("logs")?;
                let page = crate::hub::logs(
                    &name,
                    input.cursor,
                    input.tail,
                    input.grep.as_deref(),
                    input.wait_ms.unwrap_or(0),
                )?;
                let details = serde_json::to_value(&page)?;
                let mut text = page.lines.join("\n");
                if text.is_empty() {
                    text = "(no new lines)".to_string();
                }
                (text, details)
            }
            "stop" => {
                let name = name_required("stop")?;
                let snapshot = crate::hub::stop(&name)?;
                let details = serde_json::to_value(&snapshot)?;
                (
                    format!("Service '{name}' stopped (status: {}).", snapshot.status),
                    details,
                )
            }
            "restart" => {
                let name = name_required("restart")?;
                let snapshot = crate::hub::restart(&name)?;
                let details = serde_json::to_value(&snapshot)?;
                (
                    format!("Service '{name}' restarted (status: {}).", snapshot.status),
                    details,
                )
            }
            "describe" => {
                let name = name_required("describe")?;
                let snapshot = crate::hub::describe(&name)?;
                let details = serde_json::to_value(&snapshot)?;
                (
                    serde_json::to_string_pretty(&snapshot).unwrap_or_default(),
                    details,
                )
            }
            "send" => {
                let name = name_required("send")?;
                let mut actions = Vec::new();
                if let Some(text) = input.text.as_deref() {
                    crate::hub::send_text(&name, text, input.enter.unwrap_or(true))?;
                    actions.push(format!("sent {} byte(s) of text", text.len()));
                }
                if let Some(keys) = input.keys.as_ref()
                    && !keys.is_empty()
                {
                    crate::hub::send_keys(&name, keys)?;
                    actions.push(format!("sent keys: {}", keys.join(", ")));
                }
                if let Some(signal) = input.signal.as_deref() {
                    let mapped = match signal.to_ascii_uppercase().as_str() {
                        "SIGINT" => sysinfo::Signal::Interrupt,
                        "SIGTERM" => sysinfo::Signal::Term,
                        "SIGHUP" => sysinfo::Signal::Hangup,
                        "SIGQUIT" => sysinfo::Signal::Quit,
                        "SIGKILL" => sysinfo::Signal::Kill,
                        other => {
                            return Err(Error::validation(format!(
                                "Unknown signal '{other}'; expected SIGINT, SIGTERM, SIGHUP, \
                                 SIGQUIT, or SIGKILL"
                            )));
                        }
                    };
                    crate::hub::send_signal(&name, mapped)?;
                    actions.push(format!("sent {}", signal.to_ascii_uppercase()));
                }
                if actions.is_empty() {
                    return Err(Error::validation(
                        "hub send requires text, keys, or signal".to_string(),
                    ));
                }
                let details = serde_json::json!({
                    "schema": "pi.hub.send.v1",
                    "name": name,
                    "actions": actions,
                });
                (format!("To '{name}': {}", actions.join("; ")), details)
            }
            "jobs" => {
                let action = input
                    .action
                    .clone()
                    .unwrap_or_else(|| "list".to_string())
                    .to_ascii_lowercase();
                match action.as_str() {
                    "list" => {
                        let jobs = crate::jobs::list()?;
                        let details = serde_json::json!({
                            "schema": crate::jobs::JOB_SCHEMA,
                            "jobs": jobs,
                        });
                        let text = if jobs.is_empty() {
                            "No background jobs this session.".to_string()
                        } else {
                            let lines: Vec<String> = jobs
                                .iter()
                                .map(|job| {
                                    format!(
                                        "{}: {} (exit {})",
                                        job.id,
                                        job.status,
                                        job.exit_code.map_or_else(
                                            || "n/a".to_string(),
                                            |code| code.to_string()
                                        )
                                    )
                                })
                                .collect();
                            format!("{} background job(s):\n{}", jobs.len(), lines.join("\n"))
                        };
                        (text, details)
                    }
                    "wait" => {
                        let job_id = input.job_id.clone().ok_or_else(|| {
                            Error::validation("hub jobs wait requires jobId".to_string())
                        })?;
                        let budget = input.timeout_ms.unwrap_or(30_000).min(600_000);
                        let snapshot =
                            crate::jobs::wait(&job_id, std::time::Duration::from_millis(budget))?;
                        let details = serde_json::to_value(&snapshot)?;
                        (
                            format!(
                                "job {}: {} (exit {})",
                                snapshot.id,
                                snapshot.status,
                                snapshot
                                    .exit_code
                                    .map_or_else(|| "n/a".to_string(), |code| code.to_string())
                            ),
                            details,
                        )
                    }
                    "cancel" => {
                        let job_id = input.job_id.clone().ok_or_else(|| {
                            Error::validation("hub jobs cancel requires jobId".to_string())
                        })?;
                        let snapshot = crate::jobs::cancel(&job_id)?;
                        let details = serde_json::to_value(&snapshot)?;
                        (format!("job {}: {}", snapshot.id, snapshot.status), details)
                    }
                    other => {
                        return Err(Error::validation(format!(
                            "Unknown jobs action '{other}'; expected list, wait, or cancel"
                        )));
                    }
                }
            }
            "agent" => {
                let action = input
                    .action
                    .clone()
                    .unwrap_or_else(|| "roster".to_string())
                    .to_ascii_lowercase();
                Self::dispatch_agent(&action, input)?
            }
            other => {
                return Err(Error::validation(format!(
                    "Unknown hub op '{other}'; expected start, ps, logs, stop, restart, \
                     describe, send, jobs, or agent"
                )));
            }
        };
        Ok((text, details))
    }

    /// Agent-hub action group (bd-cv653.5.3): roster / transcript / steer /
    /// kill / revive / send / inbox over this session's subagent children.
    #[allow(clippy::too_many_lines)] // Cohesive action dispatcher: one arm per hub op.
    fn dispatch_agent(action: &str, input: &HubInput) -> Result<(String, serde_json::Value)> {
        let id_required = |action: &str| -> Result<String> {
            input
                .name
                .clone()
                .filter(|name| !name.trim().is_empty())
                .ok_or_else(|| {
                    Error::validation(format!("hub agent {action} requires name (child run id)"))
                })
        };
        let from = input.from.clone().unwrap_or_else(|| "parent".to_string());
        let (text, details) = match action {
            "roster" => {
                let entries = crate::agent_hub::registry()
                    .lock()
                    .map_err(|_| Error::tool("hub", "agent registry lock poisoned"))?
                    .roster();
                let details = serde_json::json!({
                    "schema": "pi.agent-hub.roster/v1",
                    "children": entries,
                });
                let text = if entries.is_empty() {
                    "No subagent children this session.".to_string()
                } else {
                    let lines: Vec<String> = entries
                        .iter()
                        .map(|e| {
                            format!(
                                "{} [{}] {} (pid {}, {} bytes out)",
                                e.id,
                                e.status.as_str(),
                                e.task,
                                e.pid
                                    .map_or_else(|| "n/a".to_string(), |pid| pid.to_string()),
                                e.output_bytes
                            )
                        })
                        .collect();
                    format!("{} child run(s):\n{}", entries.len(), lines.join("\n"))
                };
                (text, details)
            }
            "transcript" => {
                let id = id_required("transcript")?;
                let page = crate::agent_hub::registry()
                    .lock()
                    .map_err(|_| Error::tool("hub", "agent registry lock poisoned"))?
                    .transcript_page(&id)?;
                let details = serde_json::json!({
                    "schema": "pi.agent-hub.transcript/v1",
                    "id": id,
                    "redacted": true,
                });
                let text = if page.is_empty() {
                    "(no transcript frames yet)".to_string()
                } else {
                    page
                };
                (text, details)
            }
            "steer" | "send" => {
                let id = id_required(action)?;
                let body = input
                    .text
                    .clone()
                    .filter(|t| !t.trim().is_empty())
                    .ok_or_else(|| {
                        Error::validation(format!("hub agent {action} requires text"))
                    })?;
                let message = crate::agent_hub::registry()
                    .lock()
                    .map_err(|_| Error::tool("hub", "agent registry lock poisoned"))?
                    .steer(&id, &from, &body)?;
                let details = serde_json::to_value(&message)?;
                (
                    format!("Steering queued for {id} (seq {}).", message.seq),
                    details,
                )
            }
            "kill" => {
                let id = id_required("kill")?;
                let entry = {
                    let reg = crate::agent_hub::registry()
                        .lock()
                        .map_err(|_| Error::tool("hub", "agent registry lock poisoned"))?;
                    reg.get(&id)
                        .ok_or_else(|| Error::validation(format!("hub: unknown child '{id}'")))?
                };
                if entry.status.settled() {
                    return Err(Error::validation(format!(
                        "hub: cannot kill '{id}' — already {}",
                        entry.status.as_str()
                    )));
                }
                if let Some(pid) = entry.pid {
                    // Process-tree kill so the child's bash descendants die too.
                    crate::tools::kill_process_group_tree(Some(pid));
                }
                crate::agent_hub::registry()
                    .lock()
                    .map_err(|_| Error::tool("hub", "agent registry lock poisoned"))?
                    .mark_killed(&id);
                let details = serde_json::json!({
                    "schema": "pi.agent-hub.kill/v1",
                    "id": id,
                    "killedBy": from,
                });
                (format!("Child {id} killed by operator."), details)
            }
            "revive" => {
                let id = id_required("revive")?;
                let (entry, _task) = crate::agent_hub::registry()
                    .lock()
                    .map_err(|_| Error::tool("hub", "agent registry lock poisoned"))?
                    .revive(&id)?;
                // The revived task is queued as a steering continuation: the
                // next subagent tool call can launch it; the registry entry
                // already records the lineage so the roster shows it.
                let details = serde_json::to_value(&entry)?;
                (
                    format!(
                        "Revival registered as {} (continues {id}); relaunch via the subagent tool with the recorded task.",
                        entry.id
                    ),
                    details,
                )
            }
            "inbox" => {
                let id = id_required("inbox")?;
                let messages = crate::agent_hub::registry()
                    .lock()
                    .map_err(|_| Error::tool("hub", "agent registry lock poisoned"))?
                    .inbox(&id);
                let details = serde_json::json!({
                    "schema": "pi.agent-hub.inbox/v1",
                    "id": id,
                    "messages": messages,
                });
                let text = if messages.is_empty() {
                    format!("{id}: inbox empty.")
                } else {
                    let lines: Vec<String> = messages
                        .iter()
                        .map(|m| format!("#{} from {}: {}", m.seq, m.from, m.body))
                        .collect();
                    format!("{id}: {} message(s):\n{}", messages.len(), lines.join("\n"))
                };
                (text, details)
            }
            other => {
                return Err(Error::validation(format!(
                    "Unknown agent action '{other}'; expected roster, transcript, steer, \
                     kill, revive, send, or inbox"
                )));
            }
        };
        Ok((text, details))
    }
}

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
    before_persist_hook: Option<Arc<dyn Fn() + Send + Sync>>,
    mutation_recorder: Option<Arc<crate::undo::FileMutationRecorder>>,
    workspace: WorkspaceHandle,
}

impl EditTool {
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            workspace: WorkspaceHandle::default(),
            before_persist_hook: None,
            mutation_recorder: None,
        }
    }

    /// Attach the session workspace root handle (bd-cv653.3.12).
    #[must_use]
    pub fn with_workspace(mut self, workspace: WorkspaceHandle) -> Self {
        self.workspace = workspace;
        self
    }

    /// Attach the session's undo recorder (bd-cv653.3.13).
    #[must_use]
    pub fn with_mutation_recorder(
        mut self,
        recorder: Option<Arc<crate::undo::FileMutationRecorder>>,
    ) -> Self {
        self.mutation_recorder = recorder;
        self
    }

    #[cfg(test)]
    fn with_before_persist_hook(cwd: &Path, hook: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            workspace: WorkspaceHandle::default(),
            before_persist_hook: Some(Arc::new(hook)),
            mutation_recorder: None,
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

#[cfg(test)]
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

/// Outcome of the shared unique-replacement core.
struct TextReplacementOutcome {
    /// BOM-restored content ready to persist.
    final_content: String,
    /// LF-normalized pre-edit content (diff input).
    before_normalized: String,
    /// LF-normalized post-edit content (diff input).
    after_normalized: String,
}

/// Shared text-replacement core for local and `ssh://` edits
/// (bd-cv653.6.5): exactly one fuzzy-unique match of `old_text` replaced
/// with `new_text`, preserving BOM, line endings, and unmatched bytes
/// verbatim. Local edits feed the result through atomic-replace with a CAS
/// expectation; remote edits stage the result atomically over ssh.
fn apply_unique_text_replacement(
    raw: &[u8],
    old_text: &str,
    new_text: &str,
    display_path: &str,
    tool: &'static str,
) -> Result<TextReplacementOutcome> {
    let raw_content = String::from_utf8(raw.to_vec()).map_err(|_| {
        Error::tool(
            tool,
            "File contains invalid UTF-8 characters and cannot be safely edited as text."
                .to_string(),
        )
    })?;

    // Strip BOM before matching (LLM won't include invisible BOM in oldText).
    let (content_no_bom, had_bom) = strip_bom(&raw_content);

    let original_ending = detect_line_ending(content_no_bom);
    let normalized_content = normalize_to_lf(content_no_bom);
    let content_for_matching = if content_no_bom.contains('\r') && !content_no_bom.contains('\n') {
        std::borrow::Cow::Owned(content_no_bom.replace('\r', "\n"))
    } else {
        std::borrow::Cow::Borrowed(content_no_bom)
    };
    let normalized_old_text = normalize_to_lf(old_text);

    if normalized_old_text.is_empty() {
        return Err(Error::tool(
            tool,
            "The old text cannot be empty. To prepend text, include the first line's content in oldText and newText.".to_string(),
        ));
    }
    if build_normalized_content(&normalized_old_text).is_empty() {
        return Err(Error::tool(
            tool,
            "The old text must include at least one non-whitespace character.".to_string(),
        ));
    }

    // Try variants of old_text to handle Unicode normalization differences
    // (NFC vs NFD) and potential input normalization (clipboard, LLM output).
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

    let Some((match_result, _precomputed_variant, matched_variant)) = best_match else {
        return Err(Error::tool(
            tool,
            format!(
                "Could not find the exact text in {}. The old text must match exactly including all whitespace and newlines.",
                display_path
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
            tool,
            format!(
                "Found {occurrences} occurrences of the text in {}. The text must be unique. Please provide more context to make it unique.",
                display_path
            ),
        ));
    }

    // Perform replacement in the original coordinate space to preserve
    // line endings and unmatched content exactly.
    let idx = match_result.index;
    let match_len = match_result.match_length;

    // Adapt new_text to match the file's line endings.
    let adapted_new_text = restore_line_endings(&normalize_to_lf(new_text), original_ending);

    let new_len = content_no_bom.len() - match_len + adapted_new_text.len();
    let mut new_content = String::with_capacity(new_len);
    new_content.push_str(&content_no_bom[..idx]);
    new_content.push_str(&adapted_new_text);
    new_content.push_str(&content_no_bom[idx + match_len..]);

    if content_no_bom.eq(&new_content) {
        return Err(Error::tool(
            tool,
            format!(
                "No changes made to {}. The replacement produced identical content. This might indicate an issue with special characters or the text not existing as expected.",
                display_path
            ),
        ));
    }

    let after_normalized = normalize_to_lf(&new_content);

    // Re-add BOM if present.
    let mut final_content = new_content;
    if had_bom {
        final_content = format!("\u{FEFF}{final_content}");
    }

    Ok(TextReplacementOutcome {
        final_content,
        before_normalized: normalized_content,
        after_normalized,
    })
}

/// Outcome of the shared hashline-op core.
struct HashlineApplyOutcome {
    /// BOM-restored content ready to persist.
    final_content: String,
    /// LF-normalized pre-edit content (diff input).
    before_normalized: String,
    /// LF-normalized post-edit content (diff input).
    after_normalized: String,
}

/// Shared hashline-op core for local and `ssh://` hashline edits
/// (bd-cv653.6.5): validates every LINE#HASH anchor against the current
/// lines, dedupes, resolves ranges, rejects overlaps, splices bottom-up,
/// then restores line endings and BOM.
fn apply_hashline_edits_to_content(
    raw_content: &str,
    edits: &[HashlineOp],
    display_path: &str,
) -> Result<HashlineApplyOutcome> {
    let (content_no_bom, had_bom) = strip_bom(raw_content);
    let original_ending = detect_line_ending(content_no_bom);
    let normalized = normalize_to_lf(content_no_bom);
    let file_lines: Vec<&str> = normalized.split('\n').collect();

    // Validate all hash references before making any changes
    if let Err(e) = collect_mismatches(edits, &file_lines, had_bom) {
        return Err(Error::tool(
            "hashline_edit",
            format!("Hash validation failed — re-read the file to get current tags.\n\n{e}"),
        ));
    }

    // Deduplicate edits
    let mut seen = std::collections::HashSet::new();
    let mut deduped_edits: Vec<&HashlineOp> = Vec::new();
    for edit in edits {
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
                            file_lines.len() - 1
                        }
                    }
                };
                resolved.push(ResolvedEdit {
                    op: "append",
                    start: idx,
                    end: idx,
                    lines: replacement_lines,
                });
            }
            _ => {}
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
                if existing.eq(&edit.lines.iter().map(String::as_str).collect::<Vec<&str>>()) {
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
                display_path
            ),
        ));
    }

    // Reconstruct content
    let after_normalized = lines.join("\n");
    let new_content = restore_line_endings(&after_normalized, original_ending);
    let mut final_content = new_content;
    if had_bom {
        final_content = format!("\u{FEFF}{final_content}");
    }

    Ok(HashlineApplyOutcome {
        final_content,
        before_normalized: normalized,
        after_normalized,
    })
}

impl EditTool {
    /// `edit` over `ssh://host/path` (bd-cv653.6.5): fetch the FULL remote
    /// content (never truncated — an editor on a truncated view would
    /// overwrite real data), run the same replacement core as local edits,
    /// then stage + rename atomically on the remote host.
    async fn execute_ssh_edit(&self, input: &EditInput) -> Result<ToolOutput> {
        let url = input.path.clone();
        let raw = asupersync::runtime::spawn_blocking_io(move || {
            crate::url_router::ssh_fetch_document(&url, READ_TOOL_MAX_BYTES)
                .map_err(std::io::Error::other)
        })
        .await
        .map_err(|e| Error::tool("edit", format!("ssh read failed: {e}")))?;
        let outcome = apply_unique_text_replacement(
            &raw,
            &input.old_text,
            &input.new_text,
            &input.path,
            "edit",
        )?;
        let write_url = input.path.clone();
        let payload = outcome.final_content.clone();
        let write_details = asupersync::runtime::spawn_blocking_io(move || {
            crate::url_router::ssh_write_document(&write_url, &payload)
                .map_err(std::io::Error::other)
        })
        .await
        .map_err(|e| Error::tool("edit", format!("ssh write failed: {e}")))?;

        let (diff, first_changed_line) =
            generate_diff_string(&outcome.before_normalized, &outcome.after_normalized);
        let mut details = serde_json::Map::new();
        details.insert("diff".to_string(), serde_json::Value::String(diff));
        if let Some(line) = first_changed_line {
            details.insert(
                "firstChangedLine".to_string(),
                serde_json::Value::Number(serde_json::Number::from(line)),
            );
        }
        if let serde_json::Value::Object(remote) = write_details {
            for (key, value) in remote {
                details.entry(key).or_insert(value);
            }
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

impl HashlineEditTool {
    /// `hashline_edit` over `ssh://host/path` (bd-cv653.6.5): full remote
    /// fetch, the same anchor-validation/splice core as local hashline
    /// edits (stale anchors rejected), atomic remote staging.
    async fn execute_ssh_hashline_edit(&self, input: &HashlineEditInput) -> Result<ToolOutput> {
        let url = input.path.clone();
        let raw = asupersync::runtime::spawn_blocking_io(move || {
            crate::url_router::ssh_fetch_document(&url, READ_TOOL_MAX_BYTES)
                .map_err(std::io::Error::other)
        })
        .await
        .map_err(|e| Error::tool("hashline_edit", format!("ssh read failed: {e}")))?;
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
        let outcome = apply_hashline_edits_to_content(&raw_content, &input.edits, &input.path)?;

        let write_url = input.path.clone();
        let payload = outcome.final_content.clone();
        let write_details = asupersync::runtime::spawn_blocking_io(move || {
            crate::url_router::ssh_write_document(&write_url, &payload)
                .map_err(std::io::Error::other)
        })
        .await
        .map_err(|e| Error::tool("hashline_edit", format!("ssh write failed: {e}")))?;
        let (diff, first_changed_line) =
            generate_diff_string(&outcome.before_normalized, &outcome.after_normalized);
        let mut details = serde_json::Map::new();
        details.insert("diff".to_string(), serde_json::Value::String(diff));
        if let Some(line) = first_changed_line {
            details.insert(
                "firstChangedLine".to_string(),
                serde_json::Value::Number(serde_json::Number::from(line)),
            );
        }
        if let serde_json::Value::Object(remote) = write_details {
            for (key, value) in remote {
                details.entry(key).or_insert(value);
            }
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
        tool_call_id: &str,
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

        // Scheme URLs route through the router — never a silent filesystem
        // fallback (bd-cv653.6.5).
        if crate::url_router::has_scheme(&input.path) {
            return self.execute_ssh_edit(&input).await;
        }

        let absolute_path = resolve_read_path(&input.path, &self.cwd);
        let absolute_path = enforce_cwd_scope(&absolute_path, &self.cwd, "edit", &self.workspace)?;

        let meta = std_metadata_async(&absolute_path).await.map_err(|err| {
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
        ensure_effective_mode_access(
            &meta,
            &absolute_path,
            UNIX_ACCESS_READ | UNIX_ACCESS_WRITE,
            "file editing",
        )
        .map_err(|err| {
            let message = match err.kind() {
                std::io::ErrorKind::PermissionDenied => {
                    format!("Permission denied: {}", input.path)
                }
                _ => format!("Failed to access file {}: {err}", input.path),
            };
            Error::tool("edit", message)
        })?;
        ensure_parent_allows_creation(&absolute_path)
            .await
            .map_err(|err| {
                let message = match err.kind() {
                    std::io::ErrorKind::PermissionDenied => {
                        format!("Permission denied: {}", input.path)
                    }
                    _ => format!("Failed to access parent directory: {err}"),
                };
                Error::tool("edit", message)
            })?;
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

        // Read the transformation source through the validated, no-follow
        // descriptor path. The digest of these exact bytes becomes the
        // optimistic-CAS expectation checked immediately before rename.
        let path_for_read = absolute_path.clone();
        let cwd_for_read = self.cwd.clone();
        let raw = asupersync::runtime::spawn_blocking_io(move || {
            read_scoped_file_capped_sync(&path_for_read, &cwd_for_read, READ_TOOL_MAX_BYTES)
        })
        .await
        .map_err(|e| Error::tool("edit", format!("Failed to read file: {e}")))?;

        if raw.len() > usize::try_from(READ_TOOL_MAX_BYTES).unwrap_or(usize::MAX) {
            return Err(Error::tool(
                "edit",
                format!("File is too large (> {READ_TOOL_MAX_BYTES} bytes)."),
            ));
        }

        let source_expectation = AtomicContentExpectation::from_bytes(&raw);
        let outcome = apply_unique_text_replacement(
            &raw,
            &input.old_text,
            &input.new_text,
            &input.path,
            "edit",
        )?;
        let final_content = outcome.final_content;
        let normalized_content = outcome.before_normalized;
        let new_content_for_diff = outcome.after_normalized;

        // Atomic write (safe improvement vs legacy, behavior-equivalent).
        let absolute_path_clone = absolute_path.clone();
        // Full canonical root set (primary + /add-dir roots) so mutations in
        // additional workspace roots are not rejected by the atomic-replace
        // containment check.
        let cwd_clone = self.workspace.snapshot_or(&self.cwd).all();
        let final_content_bytes = final_content.into_bytes();
        let before_persist_hook = self.before_persist_hook.clone();
        if let Some(recorder) = &self.mutation_recorder {
            recorder.begin_file(tool_call_id, "edit", &absolute_path);
        }
        let persisted = asupersync::runtime::spawn_blocking_io(move || {
            before_persist_hook.map_or_else(
                || {
                    atomic_replace_file_if_unchanged(
                        &absolute_path_clone,
                        &cwd_clone,
                        &final_content_bytes,
                        source_expectation,
                    )
                },
                |hook| {
                    atomic_replace_file_with(
                        &absolute_path_clone,
                        &cwd_clone,
                        &final_content_bytes,
                        Some(source_expectation),
                        move || hook(),
                    )
                },
            )
        })
        .await
        .map_err(|e| Error::tool("edit", format!("Failed to write file: {e}")));
        if let Some(recorder) = &self.mutation_recorder {
            if persisted.is_ok() {
                recorder.commit(tool_call_id);
            } else {
                recorder.abort(tool_call_id);
            }
        }
        persisted?;

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
    before_persist_hook: Option<Arc<dyn Fn() + Send + Sync>>,
    mutation_recorder: Option<Arc<crate::undo::FileMutationRecorder>>,
    workspace: WorkspaceHandle,
}

impl WriteTool {
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            workspace: WorkspaceHandle::default(),
            before_persist_hook: None,
            mutation_recorder: None,
        }
    }

    /// Attach the session workspace root handle (bd-cv653.3.12).
    #[must_use]
    pub fn with_workspace(mut self, workspace: WorkspaceHandle) -> Self {
        self.workspace = workspace;
        self
    }

    /// Attach the session's undo recorder (bd-cv653.3.13).
    #[must_use]
    pub fn with_mutation_recorder(
        mut self,
        recorder: Option<Arc<crate::undo::FileMutationRecorder>>,
    ) -> Self {
        self.mutation_recorder = recorder;
        self
    }

    #[cfg(test)]
    fn with_before_persist_hook(cwd: &Path, hook: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            workspace: WorkspaceHandle::default(),
            before_persist_hook: Some(Arc::new(hook)),
            mutation_recorder: None,
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
        tool_call_id: &str,
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

        // Scheme URLs route through the router — never a silent filesystem
        // fallback (bd-cv653.6.5).
        if crate::url_router::has_scheme(&input.path) {
            let details = crate::url_router::ssh_write_document(&input.path, &input.content)?;
            return Ok(ToolOutput {
                content: vec![ContentBlock::Text(TextContent::new(format!(
                    "Successfully wrote {} bytes to {}",
                    input.content.len(),
                    input.path
                )))],
                details: Some(details),
                is_error: false,
            });
        }

        let path = resolve_path(&input.path, &self.cwd);
        let path = enforce_cwd_scope(&path, &self.cwd, "write", &self.workspace)?;

        match std_metadata_async(&path).await {
            Ok(meta) => {
                if !meta.is_file() {
                    return Err(Error::tool(
                        "write",
                        format!("Path {} is not a regular file", path.display()),
                    ));
                }
                ensure_effective_mode_access(&meta, &path, UNIX_ACCESS_WRITE, "file writing")
                    .map_err(|err| {
                        let message = match err.kind() {
                            std::io::ErrorKind::PermissionDenied => {
                                format!("Permission denied: {}", input.path)
                            }
                            _ => format!("Failed to access file for writing: {err}"),
                        };
                        Error::tool("write", message)
                    })?;
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
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(Error::tool(
                    "write",
                    format!("Failed to access file for writing: {err}"),
                ));
            }
        }

        // Create parent directories if needed
        if let Some(parent) = path.parent() {
            ensure_parent_allows_creation(&path).await.map_err(|err| {
                let message = match err.kind() {
                    std::io::ErrorKind::PermissionDenied => {
                        format!("Permission denied: {}", input.path)
                    }
                    _ => format!("Failed to access parent directory: {err}"),
                };
                Error::tool("write", message)
            })?;
            asupersync::fs::create_dir_all(parent)
                .await
                .map_err(|e| Error::tool("write", format!("Failed to create directories: {e}")))?;
        }

        // Parity with legacy pi-mono: report JS string length (UTF-16 code units) as "bytes".
        let bytes_written = input.content.encode_utf16().count();

        // Write atomically using tempfile on a blocking thread
        let path_clone = path.clone();
        // Full canonical root set (primary + /add-dir roots) so mutations in
        // additional workspace roots are not rejected by the atomic-replace
        // containment check.
        let cwd_clone = self.workspace.snapshot_or(&self.cwd).all();
        let content_bytes = input.content.into_bytes();
        let before_persist_hook = self.before_persist_hook.clone();
        if let Some(recorder) = &self.mutation_recorder {
            recorder.begin_file(tool_call_id, "write", &path);
        }
        let persisted = asupersync::runtime::spawn_blocking_io(move || {
            before_persist_hook.map_or_else(
                || atomic_replace_file(&path_clone, &cwd_clone, &content_bytes),
                |hook| {
                    atomic_replace_file_with(
                        &path_clone,
                        &cwd_clone,
                        &content_bytes,
                        None,
                        move || hook(),
                    )
                },
            )
        })
        .await
        .map_err(|e| Error::tool("write", format!("Failed to write file: {e}")));
        if let Some(recorder) = &self.mutation_recorder {
            if persisted.is_ok() {
                recorder.commit(tool_call_id);
            } else {
                recorder.abort(tool_call_id);
            }
        }
        persisted?;

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
    artifact_root: Option<PathBuf>,
    backend: SearchBackend,
    workspace: WorkspaceHandle,
    #[cfg(test)]
    after_scope_hook: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl GrepTool {
    pub fn new(cwd: &Path) -> Self {
        Self::with_backend(cwd, SearchBackend::default())
    }

    /// Attach the session workspace root handle (bd-cv653.3.12).
    #[must_use]
    pub fn with_workspace(mut self, workspace: WorkspaceHandle) -> Self {
        self.workspace = workspace;
        self
    }

    pub(crate) fn with_backend(cwd: &Path, backend: SearchBackend) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            artifact_root: None,
            backend,
            workspace: WorkspaceHandle::default(),
            #[cfg(test)]
            after_scope_hook: None,
        }
    }

    #[cfg(test)]
    fn with_artifact_root(cwd: &Path, artifact_root: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            artifact_root: Some(artifact_root.to_path_buf()),
            backend: SearchBackend::default(),
            workspace: WorkspaceHandle::default(),
            after_scope_hook: None,
        }
    }

    #[cfg(test)]
    fn with_after_scope_hook(
        cwd: &Path,
        after_scope_hook: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            artifact_root: None,
            backend: SearchBackend::default(),
            workspace: WorkspaceHandle::default(),
            after_scope_hook: Some(Arc::new(after_scope_hook)),
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

fn path_from_rg_json(event: &serde_json::Value) -> Result<PathBuf> {
    if let Some(path) = event
        .pointer("/data/path/text")
        .and_then(serde_json::Value::as_str)
    {
        return Ok(PathBuf::from(path));
    }

    let encoded = event
        .pointer("/data/path/bytes")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            Error::tool(
                "grep",
                "ripgrep match event is missing path.text and path.bytes",
            )
        })?;
    let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encoded)
        .map_err(|error| {
            Error::tool(
                "grep",
                format!(
                    "ripgrep match event has invalid path.bytes base64: {}",
                    error_for_line_output(&error)
                ),
            )
        })?;

    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt as _;
        Ok(PathBuf::from(OsString::from_vec(bytes)))
    }

    #[cfg(not(unix))]
    String::from_utf8(bytes)
        .map(PathBuf::from)
        .map_err(|error| {
            Error::tool(
                "grep",
                format!(
                    "ripgrep match event has non-UTF-8 path.bytes on this platform: {}",
                    error_for_line_output(&error)
                ),
            )
        })
}

fn process_rg_json_match_line(
    line_res: std::io::Result<String>,
    matches: &mut Vec<(PathBuf, usize)>,
    match_count: &mut usize,
    match_limit_reached: &mut bool,
    scan_limit: usize,
) -> Result<()> {
    process_rg_json_match_line_with_filter(
        line_res,
        None,
        None,
        None,
        matches,
        match_count,
        match_limit_reached,
        scan_limit,
    )
}

#[allow(clippy::too_many_arguments)]
fn process_rg_json_match_line_with_filter(
    line_res: std::io::Result<String>,
    scoped_root: Option<&ScopedScanRoot>,
    glob_override: Option<&ignore::overrides::Override>,
    workspace_ignore: Option<&ignore::gitignore::Gitignore>,
    matches: &mut Vec<(PathBuf, usize)>,
    match_count: &mut usize,
    match_limit_reached: &mut bool,
    scan_limit: usize,
) -> Result<()> {
    if *match_limit_reached {
        return Ok(());
    }

    let line = match line_res {
        Ok(l) => l,
        Err(e) => {
            return Err(Error::tool(
                "grep",
                format!(
                    "Failed to read ripgrep JSON output: {}",
                    error_for_line_output(&e)
                ),
            ));
        }
    };
    if line.trim().is_empty() {
        return Ok(());
    }

    let event = serde_json::from_str::<serde_json::Value>(&line).map_err(|error| {
        Error::tool(
            "grep",
            format!(
                "Invalid ripgrep JSON output: {}",
                error_for_line_output(&error)
            ),
        )
    })?;

    if event.get("type").and_then(serde_json::Value::as_str) != Some("match") {
        return Ok(());
    }

    let file_path = path_from_rg_json(&event)?;
    if let Some(scoped_root) = scoped_root
        && (glob_override.is_some() || workspace_ignore.is_some())
    {
        let mapped = scoped_root.map_child_output(&file_path).map_err(|error| {
            Error::tool(
                "grep",
                format!(
                    "ripgrep returned an invalid path: {}",
                    error_for_line_output(&error)
                ),
            )
        })?;
        if let Some(glob_override) = glob_override
            && glob_override
                .matched(&mapped.logical_path, false)
                .is_ignore()
        {
            return Ok(());
        }
        if let Some(workspace_ignore) = workspace_ignore
            && workspace_ignore
                .matched_path_or_any_parents(&mapped.logical_path, false)
                .is_ignore()
        {
            return Ok(());
        }
    }
    let line_number = event
        .pointer("/data/line_number")
        .and_then(serde_json::Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
        .filter(|line_number| *line_number > 0)
        .ok_or_else(|| Error::tool("grep", "ripgrep match event is missing a valid line_number"))?;

    matches.push((file_path, line_number));
    *match_count += 1;
    if *match_count >= scan_limit {
        *match_limit_reached = true;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn drain_rg_stdout(
    stdout_rx: &std::sync::mpsc::Receiver<std::io::Result<String>>,
    scoped_root: &ScopedScanRoot,
    glob_override: Option<&ignore::overrides::Override>,
    workspace_ignore: Option<&ignore::gitignore::Gitignore>,
    matches: &mut Vec<(PathBuf, usize)>,
    match_count: &mut usize,
    match_limit_reached: &mut bool,
    scan_limit: usize,
) -> Result<()> {
    while let Ok(line_res) = stdout_rx.try_recv() {
        process_rg_json_match_line_with_filter(
            line_res,
            Some(scoped_root),
            glob_override,
            workspace_ignore,
            matches,
            match_count,
            match_limit_reached,
            scan_limit,
        )?;
        if *match_limit_reached {
            break;
        }
    }
    Ok(())
}

/// Build a gitignore matcher for the workspace root's `.gitignore`, applied
/// to scanner results as a post-filter.
///
/// External scanners run from the pinned scan root with `--no-ignore-parent`,
/// so a workspace-root `.gitignore` above the scan root is invisible to their
/// native discovery. Passing it via `--ignore-file` does not work either:
/// rg/fd anchor such patterns to the scanner's working directory, so rules
/// like `subdir/file.txt` written relative to the workspace root can never
/// match while scanning inside `subdir`. Matching mapped logical result paths
/// against a matcher rooted at the workspace keeps the workspace-relative
/// anchoring the rules were written for.
fn build_workspace_ignore_matcher(
    cwd_scope: &ScopedScanRoot,
) -> Option<ignore::gitignore::Gitignore> {
    let workspace_root = cwd_scope.logical_path();
    // Read rule content through the pinned io-space path so a concurrent
    // workspace-root swap cannot inject attacker-controlled ignore rules,
    // while anchoring the patterns at the logical root they were written for.
    let pinned_gitignore = cwd_scope.io_path().join(".gitignore");
    let content = std::fs::read_to_string(pinned_gitignore).ok()?;
    let logical_gitignore = workspace_root.join(".gitignore");
    let mut builder = ignore::gitignore::GitignoreBuilder::new(workspace_root);
    for line in content.lines() {
        let _ = builder.add_line(Some(logical_gitignore.clone()), line);
    }
    builder.build().ok()
}

/// Result of an in-process grep scan, handing the pinned roots and filters
/// back to the async caller alongside the collected matches.
struct InprocGrepScan {
    scoped_root: ScopedScanRoot,
    glob_override: Option<ignore::overrides::Override>,
    workspace_ignore: Option<ignore::gitignore::Gitignore>,
    matches: Vec<(PathBuf, usize)>,
    match_count: usize,
    limit_reached: bool,
}

/// In-process `grep` backend (bd-cv653.1.5): search with the engines ripgrep
/// is built from (`grep-regex` matcher + `grep-searcher`), walking the pinned
/// scan root with the same ignore surface the external `rg` invocation
/// consumed. Result-path filters (glob + workspace gitignore) are applied per
/// candidate file before its contents are searched — both are path-level, so
/// this yields exactly the per-match filtering of the external drain path.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn grep_inproc_scan_sync(
    scoped_root: ScopedScanRoot,
    glob_override: Option<ignore::overrides::Override>,
    workspace_ignore: Option<ignore::gitignore::Gitignore>,
    operation_cwd: &Path,
    is_directory: bool,
    pattern: &str,
    ignore_case: bool,
    literal: bool,
    scan_limit: usize,
    cancelled: &std::sync::atomic::AtomicBool,
) -> std::io::Result<InprocGrepScan> {
    use grep_searcher::{BinaryDetection, SearcherBuilder, sinks};

    let pattern_for_matcher = if literal {
        regex::escape(pattern)
    } else {
        pattern.to_string()
    };
    let matcher = grep_regex::RegexMatcherBuilder::new()
        .case_insensitive(ignore_case)
        .build(&pattern_for_matcher)
        .map_err(|err| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("regex parse error: {err}"),
            )
        })?;
    let mut searcher = SearcherBuilder::new()
        .line_number(true)
        // rg's default binary handling: stop searching a file at the first
        // NUL byte.
        .binary_detection(BinaryDetection::quit(0))
        .build();

    let mut matches: Vec<(PathBuf, usize)> = Vec::new();
    let mut match_count: usize = 0;
    let mut limit_reached = false;

    if is_directory {
        let scan_root = scoped_root.io_path();
        let global_git_ignore = resolved_global_git_ignore_for_scan(operation_cwd)?;
        let builder = recursive_scan_walk_builder(
            &scan_root,
            operation_cwd,
            RecursiveScanAccess::ReadableFiles,
            None,
            global_git_ignore.as_deref(),
        )?;
        'walk: for entry in builder.build() {
            if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "scan cancelled",
                ));
            }
            let entry = match entry {
                Ok(entry) => entry,
                // rg reports unreadable entries on stderr and keeps
                // searching; the permission preflight already failed closed
                // on anything the effective mode model forbids.
                Err(err) => {
                    tracing::debug!("in-process grep skipped an unreadable entry: {err}");
                    continue;
                }
            };
            if !entry
                .file_type()
                .is_some_and(|file_type| file_type.is_file())
            {
                continue;
            }
            let path = entry.path();
            let Ok(relative) = path.strip_prefix(&scan_root) else {
                continue;
            };
            let relative = relative.to_path_buf();
            if glob_override.is_some() || workspace_ignore.is_some() {
                let Ok(mapped) = scoped_root.map_child_output(&relative) else {
                    continue;
                };
                if let Some(glob_override) = glob_override.as_ref()
                    && glob_override
                        .matched(&mapped.logical_path, false)
                        .is_ignore()
                {
                    continue;
                }
                if let Some(workspace_ignore) = workspace_ignore.as_ref()
                    && workspace_ignore
                        .matched_path_or_any_parents(&mapped.logical_path, false)
                        .is_ignore()
                {
                    continue;
                }
            }
            let mut file_hit_limit = false;
            let search = searcher.search_path(
                &matcher,
                path,
                sinks::Lossy(|line_number, _line| {
                    matches.push((
                        relative.clone(),
                        usize::try_from(line_number).unwrap_or(usize::MAX),
                    ));
                    match_count += 1;
                    if match_count >= scan_limit {
                        file_hit_limit = true;
                        Ok(false)
                    } else {
                        Ok(true)
                    }
                }),
            );
            if let Err(err) = search {
                tracing::debug!(
                    "in-process grep skipped an unreadable file {}: {err}",
                    path_for_line_output(path)
                );
            }
            if file_hit_limit {
                limit_reached = true;
                break 'walk;
            }
        }
    } else {
        // Pinned regular file: search the held descriptor itself (dup'd, so
        // the parent's handle keeps an untouched offset) — a post-pin
        // rename/symlink swap cannot redirect the search.
        let sink = sinks::Lossy(|line_number, _line| {
            matches.push((
                PathBuf::new(),
                usize::try_from(line_number).unwrap_or(usize::MAX),
            ));
            match_count += 1;
            if match_count >= scan_limit {
                limit_reached = true;
                Ok(false)
            } else {
                Ok(true)
            }
        });
        #[cfg(unix)]
        {
            let handle = scoped_root.handle.try_clone()?;
            searcher.search_reader(&matcher, handle, sink)?;
        }
        // Windows pins parent components rather than the file itself; the
        // logical-path read matches the platform's established posture.
        #[cfg(not(unix))]
        searcher.search_path(&matcher, scoped_root.logical_path(), sink)?;
    }

    Ok(InprocGrepScan {
        scoped_root,
        glob_override,
        workspace_ignore,
        matches,
        match_count,
        limit_reached,
    })
}

fn build_grep_glob_override(
    cwd: &Path,
    glob: Option<&str>,
) -> Result<Option<ignore::overrides::Override>> {
    let Some(glob) = glob else {
        return Ok(None);
    };
    let mut builder = ignore::overrides::OverrideBuilder::new(cwd);
    builder
        .add(glob)
        .map_err(|error| Error::tool("grep", error_for_line_output(&error)))?;
    builder
        .build()
        .map(Some)
        .map_err(|error| Error::tool("grep", error_for_line_output(&error)))
}

fn drain_rg_stderr(
    stderr_rx: &std::sync::mpsc::Receiver<std::result::Result<Vec<u8>, String>>,
    stderr_bytes: &mut Vec<u8>,
) -> Result<()> {
    while let Ok(chunk_result) = stderr_rx.try_recv() {
        let chunk = chunk_result.map_err(|err| {
            Error::tool(
                "grep",
                format!("Failed to read stderr: {}", error_for_line_output(&err)),
            )
        })?;
        stderr_bytes.extend_from_slice(&chunk);
    }
    Ok(())
}

fn rg_exit_failure(status: std::process::ExitStatus, stderr: &str) -> Option<String> {
    if matches!(status.code(), Some(0 | 1)) {
        return None;
    }
    if !stderr.is_empty() {
        return Some(stderr.to_string());
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;

        if let Some(signal) = status.signal() {
            return Some(format!("ripgrep terminated by signal {signal}"));
        }
    }
    Some(status.code().map_or_else(
        || "ripgrep terminated without an exit code".to_string(),
        |code| format!("ripgrep exited with code {code}"),
    ))
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

    fn effects(&self) -> ToolEffects {
        ToolEffects::read()
    }

    #[allow(clippy::too_many_lines)]
    async fn execute(
        &self,
        tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let input_value = input.clone();
        let input: GrepInput =
            serde_json::from_value(input).map_err(|e| Error::validation(e.to_string()))?;

        if matches!(input.limit, Some(0)) {
            return Err(Error::validation(
                "`limit` must be greater than 0".to_string(),
            ));
        }

        if self.backend == SearchBackend::External && !rg_available() {
            return Err(Error::tool(
                "grep",
                "ripgrep (rg) is not available (please install ripgrep)".to_string(),
            ));
        }

        let cwd_scope = open_scoped_scan_root(&self.cwd, &self.cwd, false, None)
            .await
            .map_err(|err| {
                Error::tool(
                    "grep",
                    format!(
                        "Cannot pin working directory {} for scanning: {}",
                        path_for_line_output(&self.cwd),
                        error_for_line_output(&err)
                    ),
                )
            })?;
        let operation_cwd = cwd_scope.io_path();

        let search_dir = input.path.as_deref().unwrap_or(".");
        let lexical_search_path = resolve_read_path(search_dir, &self.cwd);
        let search_path =
            enforce_cwd_scope(&lexical_search_path, &self.cwd, "grep", &self.workspace)?;
        ensure_scan_path_ancestors_searchable(&lexical_search_path, &search_path)
            .await
            .map_err(|err| {
                Error::tool(
                    "grep",
                    format!(
                        "Cannot access path {}: {}",
                        path_for_line_output(&search_path),
                        error_for_line_output(&err)
                    ),
                )
            })?;

        let search_metadata = std_metadata_async(&search_path).await.map_err(|e| {
            Error::tool(
                "grep",
                format!(
                    "Cannot access path {}: {}",
                    path_for_line_output(&search_path),
                    error_for_line_output(&e)
                ),
            )
        })?;
        let is_directory = search_metadata.is_dir();
        let required_access = if is_directory {
            UNIX_ACCESS_READ | UNIX_ACCESS_SEARCH
        } else {
            UNIX_ACCESS_READ
        };
        ensure_effective_mode_access(
            &search_metadata,
            &search_path,
            required_access,
            "content scanning",
        )
        .map_err(|err| {
            Error::tool(
                "grep",
                format!(
                    "Cannot access path {}: {}",
                    path_for_line_output(&search_path),
                    error_for_line_output(&err)
                ),
            )
        })?;
        #[cfg(test)]
        let after_scope_hook = self.after_scope_hook.clone();
        #[cfg(not(test))]
        let after_scope_hook = None;
        let scoped_root = open_scoped_scan_root(&search_path, &self.cwd, true, after_scope_hook)
            .await
            .map_err(|err| {
                Error::tool(
                    "grep",
                    format!(
                        "Cannot pin path {} for scanning: {}",
                        path_for_line_output(&search_path),
                        error_for_line_output(&err)
                    ),
                )
            })?;
        let scan_io_path = scoped_root.io_path();
        if is_directory {
            ensure_recursive_scan_access(
                &scan_io_path,
                &operation_cwd,
                "recursive content scanning",
                RecursiveScanAccess::ReadableFiles,
                input.glob.clone(),
            )
            .await
            .map_err(|err| {
                let message = if err.kind() == std::io::ErrorKind::PermissionDenied {
                    format!(
                        "Permission denied while scanning descendants of {}",
                        path_for_line_output(&search_path)
                    )
                } else {
                    format!(
                        "Cannot scan path {}: {}",
                        path_for_line_output(&search_path),
                        error_for_line_output(&err)
                    )
                };
                Error::tool("grep", message)
            })?;
        }

        let context_value = input.context.unwrap_or(0);
        let effective_limit = input.limit.unwrap_or(DEFAULT_GREP_LIMIT).max(1);
        // Overfetch one match so limit notices only appear after confirmed overflow.
        let scan_limit = effective_limit.saturating_add(1);
        let cache_key = tool_cache_key("grep", &self.cwd, &input_value);
        let cache_mode = if is_directory {
            ToolCacheFingerprintMode::DirectoryRecursive
        } else {
            ToolCacheFingerprintMode::FileContent
        };
        let recursive_cache_access = is_directory.then_some(RecursiveScanAccess::ReadableFiles);
        let cache_deps = cache_dependencies_for_scoped_scan(
            &scoped_root,
            &cwd_scope,
            cache_mode,
            recursive_cache_access,
        );
        if let Some(output) = cached_tool_output(&cache_key, cache_deps.as_deref()) {
            return Ok(output);
        }

        // The scanner runs from the pinned search root, while grep globs are
        // defined relative to the command cwd. Apply that glob to mapped
        // logical result paths so a descriptor path cannot change its
        // slash-containing semantics.
        let glob_override =
            build_grep_glob_override(cwd_scope.logical_path(), input.glob.as_deref())?;

        // Mirror find-tool behavior: the walk discovers .gitignore files
        // natively, while workspace-root rules above the scan root are
        // applied as a result-path filter (see build_workspace_ignore_matcher
        // for why --ignore-file cannot express them). Explicit file operands
        // bypass ignore filtering entirely, so neither applies to
        // single-file scans.
        let workspace_ignore = if is_directory {
            build_workspace_ignore_matcher(&cwd_scope)
        } else {
            None
        };

        let mut matches: Vec<(PathBuf, usize)> = Vec::new();
        let mut match_count: usize = 0;
        let mut match_scan_limit_reached = false;

        let scoped_root = if self.backend == SearchBackend::Inproc {
            let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let cancel_for_scan = Arc::clone(&cancelled);
            let operation_cwd_owned = operation_cwd.clone();
            let pattern = input.pattern.clone();
            let ignore_case = input.ignore_case.unwrap_or(false);
            let literal = input.literal.unwrap_or(false);
            let mut scan = Box::pin(asupersync::runtime::spawn_blocking_io(move || {
                grep_inproc_scan_sync(
                    scoped_root,
                    glob_override,
                    workspace_ignore,
                    &operation_cwd_owned,
                    is_directory,
                    &pattern,
                    ignore_case,
                    literal,
                    scan_limit,
                    &cancel_for_scan,
                )
            }));
            let tick = Duration::from_millis(10);
            let outcome = loop {
                let agent_cx = AgentCx::for_current_or_request();
                let cx = agent_cx.cx();
                if cx.checkpoint().is_err() {
                    cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
                    return Err(Error::tool("grep", "Command cancelled"));
                }
                let now = cx.timer_driver().map_or_else(wall_now, |timer| timer.now());
                let sleeper = Box::pin(sleep(now, tick));
                match futures::future::select(scan, sleeper).await {
                    futures::future::Either::Left((result, _)) => break result,
                    futures::future::Either::Right(((), pending)) => scan = pending,
                }
            };
            let outcome =
                outcome.map_err(|err| Error::tool("grep", error_for_line_output(&err)))?;
            matches = outcome.matches;
            match_count = outcome.match_count;
            // outcome.limit_reached is equivalent to match_count > effective_limit
            // (scan_limit is effective_limit + 1), which the shared truncation
            // check below re-derives from match_count.
            outcome.scoped_root
        } else {
            let mut args: Vec<OsString> = vec![
                OsString::from("--json"),
                OsString::from("--line-number"),
                OsString::from("--color=never"),
                OsString::from("--hidden"),
                OsString::from("--no-config"),
                OsString::from("--no-follow"),
                OsString::from("--no-ignore-parent"),
                OsString::from("--no-require-git"),
                // Prevent massive JSON lines from minified files causing OOM
                OsString::from("--max-columns=10000"),
            ];

            if input.ignore_case.unwrap_or(false) {
                args.push(OsString::from("--ignore-case"));
            }
            if input.literal.unwrap_or(false) {
                args.push(OsString::from("--fixed-strings"));
            }
            if is_directory {
                let root_gitignore = scoped_root.child_operand().join(".gitignore");
                if scoped_root.logical_path() != cwd_scope.logical_path()
                    && scan_io_path.join(".gitignore").exists()
                {
                    args.push(OsString::from("--ignore-file"));
                    args.push(root_gitignore.as_os_str().to_owned());
                }
            }

            args.push(OsString::from("--"));
            args.push(OsString::from(&input.pattern));
            args.push(if is_directory {
                scoped_root.child_operand().into_os_string()
            } else {
                scoped_root.file_child_operand().into_os_string()
            });

            let rg_cmd = find_rg_binary().ok_or_else(|| {
                Error::tool(
                    "grep",
                    "rg is not available (please install ripgrep or rg)".to_string(),
                )
            })?;

            // A child cannot chdir into a regular file, so single-file scans run
            // from the pinned workspace root and read the pinned file descriptor
            // via stdin (`-` operand) instead.
            let child_cwd = if is_directory {
                scan_io_path.clone()
            } else {
                operation_cwd.clone()
            };
            let child_stdin = if is_directory {
                cwd_scope.child_stdin()
            } else {
                scoped_root.child_stdin()
            };
            let mut child = command_with_default_sigpipe_in_dir(rg_cmd, &child_cwd)
                .map_err(|e| {
                    Error::tool(
                        "grep",
                        format!("Failed to prepare ripgrep: {}", error_for_line_output(&e)),
                    )
                })?
                .args(args)
                .current_dir(&child_cwd)
                .stdin(child_stdin.map_err(|error| {
                    Error::tool(
                        "grep",
                        format!(
                            "Failed to pin ripgrep workspace: {}",
                            error_for_line_output(&error)
                        ),
                    )
                })?)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|e| {
                    Error::tool(
                        "grep",
                        format!("Failed to run ripgrep: {}", error_for_line_output(&e)),
                    )
                })?;

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
                    &scoped_root,
                    glob_override.as_ref(),
                    workspace_ignore.as_ref(),
                    &mut matches,
                    &mut match_count,
                    &mut match_scan_limit_reached,
                    scan_limit,
                )?;
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
                    Err(e) => return Err(Error::tool("grep", error_for_line_output(&e))),
                }
            };

            drain_rg_stdout(
                &stdout_rx,
                &scoped_root,
                glob_override.as_ref(),
                workspace_ignore.as_ref(),
                &mut matches,
                &mut match_count,
                &mut match_scan_limit_reached,
                scan_limit,
            )?;

            let completed_status = if match_scan_limit_reached || cx_cancelled {
                // Avoid buffering unbounded stdout/stderr once we've hit the match limit.
                // `kill()` terminates the process, and we reap it in a background thread
                // so the stdout reader threads can exit promptly without blocking this task.
                let _ = guard.kill();
                // Drop any buffered stdout/stderr lines that were queued before termination.
                while stdout_rx.try_recv().is_ok() {}
                while stderr_rx.try_recv().is_ok() {}
                None
            } else {
                Some(exit_status.expect("rg exit status"))
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
                        &scoped_root,
                        glob_override.as_ref(),
                        workspace_ignore.as_ref(),
                        &mut matches,
                        &mut match_count,
                        &mut match_scan_limit_reached,
                        scan_limit,
                    )?;
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
                    &scoped_root,
                    glob_override.as_ref(),
                    workspace_ignore.as_ref(),
                    &mut matches,
                    &mut match_count,
                    &mut match_scan_limit_reached,
                    scan_limit,
                )?;
            }
            drain_rg_stderr(&stderr_rx, &mut stderr_bytes)?;

            let stderr_text = diagnostic_for_line_output(
                &stderr_bytes,
                stderr_bytes.len() as u64 > READ_TOOL_MAX_BYTES,
            );
            if !match_scan_limit_reached
                && let Some(message) =
                    completed_status.and_then(|status| rg_exit_failure(status, &stderr_text))
            {
                return Err(Error::tool("grep", message));
            }
            scoped_root
        };

        let match_limit_reached = match_count > effective_limit;
        if match_limit_reached {
            matches.truncate(effective_limit);
            match_count = effective_limit;
        }

        if match_count == 0 {
            let output = ToolOutput {
                content: vec![ContentBlock::Text(TextContent::new("No matches found"))],
                details: None,
                is_error: false,
            };
            cache_tool_output(
                cache_key,
                stable_cache_dependencies_for_scoped_scan(
                    &scoped_root,
                    &cwd_scope,
                    cache_mode,
                    recursive_cache_access,
                    cache_deps.as_deref(),
                ),
                &output,
            );
            return Ok(output);
        }

        let mut output_builder = HeadTruncatingLineWriter::new(DEFAULT_MAX_BYTES);
        let mut artifact_source = String::new();
        let mut lines_truncated = false;

        // Group matches by file to merge overlapping context windows
        let mut file_order: Vec<PathBuf> = Vec::new();
        let mut matches_by_file: HashMap<PathBuf, Vec<usize>> = HashMap::new();
        let mut logical_paths_by_file: HashMap<PathBuf, PathBuf> = HashMap::new();
        for (child_path, line_number) in &matches {
            let mapped = scoped_root.map_child_output(child_path).map_err(|error| {
                Error::tool(
                    "grep",
                    format!(
                        "ripgrep returned an invalid path: {}",
                        error_for_line_output(&error)
                    ),
                )
            })?;
            if !matches_by_file.contains_key(&mapped.read_path) {
                file_order.push(mapped.read_path.clone());
                logical_paths_by_file.insert(mapped.read_path.clone(), mapped.logical_path.clone());
            }
            matches_by_file
                .entry(mapped.read_path)
                .or_default()
                .push(*line_number);
        }

        for file_path in file_order {
            let Some(mut match_lines) = matches_by_file.remove(&file_path) else {
                continue;
            };
            let logical_path = logical_paths_by_file
                .remove(&file_path)
                .ok_or_else(|| Error::tool("grep", "missing logical scanner result path"))?;
            let relative_path = format_grep_path(&logical_path, cwd_scope.logical_path());
            // A pinned regular-file root renders context from the held
            // descriptor so a post-pin rename/symlink swap of the logical
            // path cannot redirect or break the read.
            let lines = if scoped_root.is_file_root() {
                get_pinned_file_lines_async(&scoped_root).await
            } else {
                get_file_lines_async(&file_path, &operation_cwd).await
            };

            if lines.is_empty() {
                if let Some(first_match) = match_lines.first() {
                    let line = format!(
                        "{relative_path}:{first_match}: (unable to read file or too large)"
                    );
                    output_builder.push_line(&line);
                    append_artifact_source_line(&mut artifact_source, &line);
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

                if let Some(last_block) = blocks.last_mut()
                    && start <= last_block.1.saturating_add(1)
                {
                    last_block.1 = last_block.1.max(end);
                    continue;
                }
                blocks.push((start, end));
            }

            for (i, (start, end)) in blocks.into_iter().enumerate() {
                if i > 0 {
                    output_builder.push_line("--");
                    append_artifact_source_line(&mut artifact_source, "--");
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
                        let line = if match_lines.binary_search(&current).is_ok() {
                            format!("{relative_path}:{tag}: {}", truncated.text)
                        } else {
                            format!("{relative_path}-{tag}- {}", truncated.text)
                        };
                        output_builder.push_line(&line);
                        append_artifact_source_line(&mut artifact_source, &line);
                    } else if match_lines.binary_search(&current).is_ok() {
                        let line = format!("{relative_path}:{current}: {}", truncated.text);
                        output_builder.push_line(&line);
                        append_artifact_source_line(&mut artifact_source, &line);
                    } else {
                        let line = format!("{relative_path}-{current}- {}", truncated.text);
                        output_builder.push_line(&line);
                        append_artifact_source_line(&mut artifact_source, &line);
                    }
                }
            }
        }

        // Apply byte truncation while writing, avoiding a second joined copy.
        let mut truncation = output_builder.finish();

        let mut output = std::mem::take(&mut truncation.content);
        let mut notices: Vec<String> = Vec::new();
        let mut details_map = serde_json::Map::new();

        if match_limit_reached {
            notices.push(format!(
                "{effective_limit} matches limit reached. Use limit={} for more, or refine pattern",
                effective_limit.saturating_mul(2)
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

        let mut details = if details_map.is_empty() {
            None
        } else {
            Some(serde_json::Value::Object(details_map))
        };

        attach_text_artifact_if_needed_with_root(
            self.artifact_root.as_deref(),
            &mut output,
            &mut details,
            "grep",
            tool_call_id,
            "searchResults",
            &artifact_source,
        );

        let output = ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(output))],
            details,
            is_error: false,
        };
        cache_tool_output(
            cache_key,
            stable_cache_dependencies_for_scoped_scan(
                &scoped_root,
                &cwd_scope,
                cache_mode,
                recursive_cache_access,
                cache_deps.as_deref(),
            ),
            &output,
        );
        Ok(output)
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
    rel: PathBuf,
    modified: Option<SystemTime>,
    is_dir: bool,
}

pub struct FindTool {
    cwd: PathBuf,
    artifact_root: Option<PathBuf>,
    backend: SearchBackend,
    workspace: WorkspaceHandle,
    #[cfg(test)]
    after_scope_hook: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl FindTool {
    pub fn new(cwd: &Path) -> Self {
        Self::with_backend(cwd, SearchBackend::default())
    }

    /// Attach the session workspace root handle (bd-cv653.3.12).
    #[must_use]
    pub fn with_workspace(mut self, workspace: WorkspaceHandle) -> Self {
        self.workspace = workspace;
        self
    }

    pub(crate) fn with_backend(cwd: &Path, backend: SearchBackend) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            artifact_root: None,
            backend,
            workspace: WorkspaceHandle::default(),
            #[cfg(test)]
            after_scope_hook: None,
        }
    }

    #[cfg(test)]
    fn with_after_scope_hook(
        cwd: &Path,
        after_scope_hook: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            artifact_root: None,
            backend: SearchBackend::default(),
            workspace: WorkspaceHandle::default(),
            after_scope_hook: Some(Arc::new(after_scope_hook)),
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

    fn effects(&self) -> ToolEffects {
        ToolEffects::read()
    }

    #[allow(clippy::too_many_lines)]
    async fn execute(
        &self,
        tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let input_value = input.clone();
        let input: FindInput =
            serde_json::from_value(input).map_err(|e| Error::validation(e.to_string()))?;

        if matches!(input.limit, Some(0)) {
            return Err(Error::validation(
                "`limit` must be greater than 0".to_string(),
            ));
        }

        let cwd_scope = open_scoped_scan_root(&self.cwd, &self.cwd, false, None)
            .await
            .map_err(|err| {
                Error::tool(
                    "find",
                    format!(
                        "Cannot pin working directory {} for scanning: {}",
                        path_for_line_output(&self.cwd),
                        error_for_line_output(&err)
                    ),
                )
            })?;
        let operation_cwd = cwd_scope.io_path();

        let search_dir = input.path.as_deref().unwrap_or(".");
        let lexical_search_path = resolve_read_path(search_dir, &self.cwd);
        let search_path =
            enforce_cwd_scope(&lexical_search_path, &self.cwd, "find", &self.workspace)?;
        let search_path = strip_unc_prefix(search_path);
        ensure_scan_path_ancestors_searchable(&lexical_search_path, &search_path)
            .await
            .map_err(|err| {
                Error::tool(
                    "find",
                    format!(
                        "Cannot access path {}: {}",
                        path_for_line_output(&search_path),
                        error_for_line_output(&err)
                    ),
                )
            })?;
        let effective_limit = input.limit.unwrap_or(DEFAULT_FIND_LIMIT);
        // Enumerate the complete bounded candidate set before newest-first
        // selection; stopping at the caller's result limit would make the
        // advertised global ordering depend on fd traversal order.
        let scan_limit = FIND_SCAN_HARD_LIMIT.saturating_add(1);

        let search_metadata = std_metadata_async(&search_path).await.map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                Error::tool(
                    "find",
                    format!("Path not found: {}", path_for_line_output(&search_path)),
                )
            } else {
                Error::tool(
                    "find",
                    format!(
                        "Cannot access path {}: {}",
                        path_for_line_output(&search_path),
                        error_for_line_output(&err)
                    ),
                )
            }
        })?;
        if !search_metadata.is_dir() {
            return Err(Error::tool(
                "find",
                format!("Not a directory: {}", path_for_line_output(&search_path)),
            ));
        }
        ensure_effective_mode_access(
            &search_metadata,
            &search_path,
            UNIX_ACCESS_READ | UNIX_ACCESS_SEARCH,
            "path scanning",
        )
        .map_err(|err| {
            Error::tool(
                "find",
                format!(
                    "Cannot access path {}: {}",
                    path_for_line_output(&search_path),
                    error_for_line_output(&err)
                ),
            )
        })?;
        #[cfg(test)]
        let after_scope_hook = self.after_scope_hook.clone();
        #[cfg(not(test))]
        let after_scope_hook = None;
        let scoped_root = open_scoped_scan_root(&search_path, &self.cwd, false, after_scope_hook)
            .await
            .map_err(|err| {
                Error::tool(
                    "find",
                    format!(
                        "Cannot pin path {} for scanning: {}",
                        path_for_line_output(&search_path),
                        error_for_line_output(&err)
                    ),
                )
            })?;
        let scan_io_path = scoped_root.io_path();
        ensure_recursive_scan_access(
            &scan_io_path,
            &operation_cwd,
            "recursive path scanning",
            RecursiveScanAccess::DirectoriesOnly,
            None,
        )
        .await
        .map_err(|err| {
            let message = if err.kind() == std::io::ErrorKind::PermissionDenied {
                format!(
                    "Permission denied while scanning descendants of {}",
                    path_for_line_output(&search_path)
                )
            } else {
                format!(
                    "Cannot scan path {}: {}",
                    path_for_line_output(&search_path),
                    error_for_line_output(&err)
                )
            };
            Error::tool("find", message)
        })?;

        let cache_key = tool_cache_key("find", &self.cwd, &input_value);
        let cache_mode = ToolCacheFingerprintMode::DirectoryRecursive;
        let recursive_cache_access = Some(RecursiveScanAccess::DirectoriesOnly);
        let cache_deps = cache_dependencies_for_scoped_scan(
            &scoped_root,
            &cwd_scope,
            cache_mode,
            recursive_cache_access,
        );
        if let Some(output) = cached_tool_output(&cache_key, cache_deps.as_deref()) {
            return Ok(output);
        }

        // NOTE: Both backends rely on native .gitignore discovery during the
        // walk. Workspace-root rules above the scan root are applied as a
        // post-filter on mapped result paths (see
        // build_workspace_ignore_matcher for why --ignore-file cannot express
        // them).
        let workspace_ignore = build_workspace_ignore_matcher(&cwd_scope);
        // fd matches plain `--glob` patterns against the FILENAME only, so a
        // path-shaped pattern like `src/**/*.txt` silently matches nothing
        // (fd 10.3/10.4, both platforms). For those, ask fd for a full-path
        // superset (`--full-path` with a `**/` anchor that absorbs the
        // absolute search-root prefix) and restore exact scan-root-relative
        // glob semantics with a host-side filter over the mapped logical
        // paths below. The in-process backend applies the same filename/path
        // split natively during its walk.
        let path_shaped_pattern = input.pattern.contains('/');
        let path_glob_filter = if path_shaped_pattern {
            let mut builder = ignore::overrides::OverrideBuilder::new(scoped_root.logical_path());
            builder
                .add(&input.pattern)
                .map_err(|error| Error::tool("find", error_for_line_output(&error)))?;
            Some(
                builder
                    .build()
                    .map_err(|error| Error::tool("find", error_for_line_output(&error)))?,
            )
        } else {
            None
        };

        let raw_paths: Vec<PathBuf> = if self.backend == SearchBackend::Inproc {
            let scan_root = scan_io_path.clone();
            let walk_cwd = operation_cwd.clone();
            let pattern = input.pattern.clone();
            let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let cancel_for_walk = Arc::clone(&cancelled);
            let mut walk = Box::pin(asupersync::runtime::spawn_blocking_io(move || {
                find_inproc_scan_sync(
                    &scan_root,
                    &walk_cwd,
                    &pattern,
                    path_shaped_pattern,
                    scan_limit,
                    &cancel_for_walk,
                )
            }));
            let tick = Duration::from_millis(10);
            let start_time = std::time::Instant::now();
            let outcome = loop {
                let agent_cx = AgentCx::for_current_or_request();
                let cx = agent_cx.cx();
                if cx.checkpoint().is_err() {
                    cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
                    return Err(Error::tool("find", "Command cancelled"));
                }
                if start_time.elapsed().as_millis() > 60_000 {
                    cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
                    return Err(Error::tool("find", "Command timed out after 60 seconds"));
                }
                let now = cx.timer_driver().map_or_else(wall_now, |timer| timer.now());
                let sleeper = Box::pin(sleep(now, tick));
                match futures::future::select(walk, sleeper).await {
                    futures::future::Either::Left((result, _)) => break result,
                    futures::future::Either::Right(((), pending)) => walk = pending,
                }
            };
            outcome.map_err(|err| {
                Error::tool(
                    "find",
                    format!("in-process find failed: {}", error_for_line_output(&err)),
                )
            })?
        } else {
            self.run_external_fd_scan(
                &input,
                &scoped_root,
                &cwd_scope,
                &scan_io_path,
                scan_limit,
                path_shaped_pattern,
            )
            .await?
        };

        if raw_paths.is_empty() {
            let output = ToolOutput {
                content: vec![ContentBlock::Text(TextContent::new(
                    "No files found matching pattern",
                ))],
                details: None,
                is_error: false,
            };
            cache_tool_output(
                cache_key,
                stable_cache_dependencies_for_scoped_scan(
                    &scoped_root,
                    &cwd_scope,
                    cache_mode,
                    recursive_cache_access,
                    cache_deps.as_deref(),
                ),
                &output,
            );
            return Ok(output);
        }

        let mut entries: Vec<FindEntry> = Vec::new();
        for raw_path in raw_paths {
            let mapped = scoped_root.map_child_output(&raw_path).map_err(|error| {
                Error::tool(
                    "find",
                    format!(
                        "fd returned an invalid path: {}",
                        error_for_line_output(&error)
                    ),
                )
            })?;
            let rel = mapped.relative;
            let full_path = mapped.read_path;
            // fd was invoked with --no-follow. Inspect the directory entry
            // itself as well, otherwise `is_dir`/`metadata` follows a symlink
            // or junction after the scan and leaks target type/mtime into the
            // output and sort order.
            let entry_metadata = std::fs::symlink_metadata(&full_path).ok();
            let is_dir = entry_metadata
                .as_ref()
                .is_some_and(std::fs::Metadata::is_dir);
            if let Some(workspace_ignore) = workspace_ignore.as_ref()
                && workspace_ignore
                    .matched_path_or_any_parents(&mapped.logical_path, is_dir)
                    .is_ignore()
            {
                continue;
            }
            // Exact scan-root-relative glob semantics for path-shaped
            // patterns; the external backend only produced the full-path
            // superset (see above).
            if let Some(path_glob_filter) = path_glob_filter.as_ref()
                && path_glob_filter
                    .matched(&mapped.logical_path, is_dir)
                    .is_ignore()
            {
                continue;
            }

            let modified = entry_metadata.and_then(|meta| meta.modified().ok());
            entries.push(FindEntry {
                rel,
                modified,
                is_dir,
            });
        }
        drop(path_glob_filter);
        drop(workspace_ignore);

        run_find_output_pipeline(
            entries,
            effective_limit,
            cache_key,
            &scoped_root,
            &cwd_scope,
            cache_mode,
            recursive_cache_access,
            cache_deps.as_deref(),
            self.artifact_root.as_deref(),
            tool_call_id,
        )
    }
}

impl FindTool {
    /// External `fd` backend (escape hatch): spawn fd with the historical
    /// argument surface and return scan-root-relative result paths.
    #[allow(clippy::too_many_lines)]
    async fn run_external_fd_scan(
        &self,
        input: &FindInput,
        scoped_root: &ScopedScanRoot,
        cwd_scope: &ScopedScanRoot,
        scan_io_path: &Path,
        scan_limit: usize,
        path_shaped_pattern: bool,
    ) -> Result<Vec<PathBuf>> {
        let fd_cmd = find_fd_binary().ok_or_else(|| {
            Error::tool(
                "find",
                "fd is not available (please install fd-find or fd)".to_string(),
            )
        })?;

        // Build fd arguments
        let mut args: Vec<OsString> = vec![
            OsString::from("--glob"),
            OsString::from("--color=never"),
            OsString::from("--hidden"),
            OsString::from("--no-follow"),
            OsString::from("--no-ignore-parent"),
            OsString::from("--no-require-git"),
            // Filenames may contain newlines on Unix. NUL-delimited records
            // prevent one real entry from being split into attacker-chosen
            // path fragments during postprocessing.
            OsString::from("--print0"),
            OsString::from("--max-results"),
            OsString::from(scan_limit.to_string()),
        ];

        let root_gitignore = scoped_root.child_operand().join(".gitignore");
        if scoped_root.logical_path() != cwd_scope.logical_path()
            && scan_io_path.join(".gitignore").exists()
        {
            args.push(OsString::from("--ignore-file"));
            args.push(root_gitignore.as_os_str().to_owned());
        }

        if path_shaped_pattern {
            args.push(OsString::from("--full-path"));
        }
        args.push(OsString::from("--"));
        if path_shaped_pattern {
            let anchored = input.pattern.trim_start_matches('/');
            if anchored.starts_with("**/") {
                args.push(OsString::from(anchored));
            } else {
                args.push(OsString::from(format!("**/{anchored}")));
            }
        } else {
            args.push(OsString::from(&input.pattern));
        }
        args.push(scoped_root.child_operand().into_os_string());

        let mut child = command_with_default_sigpipe_in_dir(fd_cmd, scan_io_path)
            .map_err(|e| {
                Error::tool(
                    "find",
                    format!("Failed to prepare fd: {}", error_for_line_output(&e)),
                )
            })?
            .args(args)
            .current_dir(scan_io_path)
            .stdin(cwd_scope.child_stdin().map_err(|error| {
                Error::tool(
                    "find",
                    format!(
                        "Failed to pin fd workspace: {}",
                        error_for_line_output(&error)
                    ),
                )
            })?)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                Error::tool(
                    "find",
                    format!("Failed to run fd: {}", error_for_line_output(&e)),
                )
            })?;

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
                Err(e) => return Err(Error::tool("find", error_for_line_output(&e))),
            }
        };

        let stdout_bytes = stdout_handle
            .join()
            .map_err(|_| Error::tool("find", "fd stdout reader thread panicked"))?
            .map_err(|err| {
                Error::tool(
                    "find",
                    format!("Failed to read fd stdout: {}", error_for_line_output(&err)),
                )
            })?;
        let stderr_bytes = stderr_handle
            .join()
            .map_err(|_| Error::tool("find", "fd stderr reader thread panicked"))?
            .map_err(|err| {
                Error::tool(
                    "find",
                    format!("Failed to read fd stderr: {}", error_for_line_output(&err)),
                )
            })?;

        if cx_cancelled {
            return Err(Error::tool("find", "Command cancelled"));
        }
        if timed_out {
            return Err(Error::tool("find", "Command timed out after 60 seconds"));
        }
        let status = status.expect("fd exit status after successful completion");

        if stdout_bytes.len() as u64 > READ_TOOL_MAX_BYTES {
            return Err(Error::tool(
                "find",
                "fd output exceeded the safe capture limit; lower `limit` or narrow the pattern",
            ));
        }
        let stderr = diagnostic_for_line_output(
            &stderr_bytes,
            stderr_bytes.len() as u64 > READ_TOOL_MAX_BYTES,
        );

        if !status.success() {
            if status.code() == Some(1) && stderr.is_empty() && stdout_bytes.is_empty() {
                // fd uses exit code 1 for "no matches"; treat as empty result.
            } else {
                let code = status.code().unwrap_or(1);
                let msg = if stderr.is_empty() {
                    if stdout_bytes.is_empty() {
                        format!("fd exited with code {code}")
                    } else {
                        format!("fd exited with code {code} after producing partial output")
                    }
                } else {
                    stderr
                };
                return Err(Error::tool("find", msg));
            }
        }

        let mut raw_paths = Vec::new();
        for raw_entry in stdout_bytes.split(|byte| *byte == b'\0') {
            if raw_entry.is_empty() {
                continue;
            }
            // `fd --print0` preserves every valid filename byte other than
            // the impossible NUL byte, including leading/trailing whitespace
            // and embedded newlines.
            #[cfg(unix)]
            let raw_path = {
                use std::os::unix::ffi::OsStringExt as _;
                PathBuf::from(OsString::from_vec(raw_entry.to_vec()))
            };
            #[cfg(not(unix))]
            let raw_path = PathBuf::from(String::from_utf8_lossy(raw_entry).into_owned());
            raw_paths.push(raw_path);
        }
        Ok(raw_paths)
    }
}

/// Shared find output assembly: sort newest-first, apply limits and
/// truncation, spill artifacts, and cache the rendered output.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn run_find_output_pipeline(
    mut entries: Vec<FindEntry>,
    effective_limit: usize,
    cache_key: String,
    scoped_root: &ScopedScanRoot,
    cwd_scope: &ScopedScanRoot,
    cache_mode: ToolCacheFingerprintMode,
    recursive_cache_access: Option<RecursiveScanAccess>,
    cache_deps: Option<&[ToolCacheDependency]>,
    artifact_root: Option<&Path>,
    tool_call_id: &str,
) -> Result<ToolOutput> {
    {
        if entries.len() > FIND_SCAN_HARD_LIMIT {
            return Err(Error::tool(
                "find",
                format!(
                    "find scan exceeded the {FIND_SCAN_HARD_LIMIT} candidate safety limit; narrow the path or pattern"
                ),
            ));
        }

        entries.sort_by(|a, b| {
            let ordering = match (&a.modified, &b.modified) {
                (Some(a_time), Some(b_time)) => b_time.cmp(a_time),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => Ordering::Equal,
            };
            ordering.then_with(|| compare_paths_for_line_output(&a.rel, &b.rel))
        });

        if entries.is_empty() {
            let output = ToolOutput {
                content: vec![ContentBlock::Text(TextContent::new(
                    "No files found matching pattern",
                ))],
                details: None,
                is_error: false,
            };
            cache_tool_output(
                cache_key,
                stable_cache_dependencies_for_scoped_scan(
                    scoped_root,
                    cwd_scope,
                    cache_mode,
                    recursive_cache_access,
                    cache_deps,
                ),
                &output,
            );
            return Ok(output);
        }

        let result_limit_reached = entries.len() > effective_limit;
        let mut output_builder = HeadTruncatingLineWriter::new(DEFAULT_MAX_BYTES);
        let mut artifact_source = String::new();
        for entry in entries.into_iter().take(effective_limit) {
            let escaped_path = path_for_line_output(&entry.rel);
            let line = if entry.is_dir {
                format!("{escaped_path}/")
            } else {
                escaped_path
            };
            output_builder.push_line(&line);
            append_artifact_source_line(&mut artifact_source, &line);
        }
        let mut truncation = output_builder.finish();

        let mut result_output = std::mem::take(&mut truncation.content);
        let mut notices: Vec<String> = Vec::new();
        let mut details_map = serde_json::Map::new();

        if result_limit_reached {
            notices.push(format!(
                "{effective_limit} results limit reached. Use limit={} for more, or refine pattern",
                effective_limit.saturating_mul(2)
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

        let mut details = if details_map.is_empty() {
            None
        } else {
            Some(serde_json::Value::Object(details_map))
        };

        attach_text_artifact_if_needed_with_root(
            artifact_root,
            &mut result_output,
            &mut details,
            "find",
            tool_call_id,
            "fileResults",
            &artifact_source,
        );

        let output = ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(result_output))],
            details,
            is_error: false,
        };
        cache_tool_output(
            cache_key,
            stable_cache_dependencies_for_scoped_scan(
                scoped_root,
                cwd_scope,
                cache_mode,
                recursive_cache_access,
                cache_deps,
            ),
            &output,
        );
        Ok(output)
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
    artifact_root: Option<PathBuf>,
    cwd: PathBuf,
    workspace: WorkspaceHandle,
    #[cfg(test)]
    after_scope_hook: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl LsTool {
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            artifact_root: None,
            workspace: WorkspaceHandle::default(),
            #[cfg(test)]
            after_scope_hook: None,
        }
    }

    /// Attach the session workspace root handle (bd-cv653.3.12).
    #[must_use]
    pub fn with_workspace(mut self, workspace: WorkspaceHandle) -> Self {
        self.workspace = workspace;
        self
    }

    #[cfg(test)]
    fn with_artifact_root(cwd: &Path, artifact_root: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            artifact_root: Some(artifact_root.to_path_buf()),
            workspace: WorkspaceHandle::default(),
            #[cfg(test)]
            after_scope_hook: None,
        }
    }
    #[cfg(test)]
    fn with_after_scope_hook(
        cwd: &Path,
        after_scope_hook: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            artifact_root: None,
            workspace: WorkspaceHandle::default(),
            after_scope_hook: Some(Arc::new(after_scope_hook)),
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

    fn effects(&self) -> ToolEffects {
        ToolEffects::read()
    }

    async fn execute(
        &self,
        tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let input_value = input.clone();
        let input: LsInput =
            serde_json::from_value(input).map_err(|e| Error::validation(e.to_string()))?;

        if matches!(input.limit, Some(0)) {
            return Err(Error::validation(
                "`limit` must be greater than 0".to_string(),
            ));
        }

        let cwd_scope = open_scoped_scan_root(&self.cwd, &self.cwd, false, None)
            .await
            .map_err(|err| {
                Error::tool(
                    "ls",
                    format!(
                        "Cannot pin working directory {} for listing: {}",
                        path_for_line_output(&self.cwd),
                        error_for_line_output(&err)
                    ),
                )
            })?;

        let dir_path = input
            .path
            .as_ref()
            .map_or_else(|| self.cwd.clone(), |p| resolve_read_path(p, &self.cwd));
        let dir_path = enforce_cwd_scope(&dir_path, &self.cwd, "list", &self.workspace)?;

        let effective_limit = input.limit.unwrap_or(DEFAULT_LS_LIMIT);

        let dir_metadata = std_metadata_async(&dir_path).await.map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                Error::tool(
                    "ls",
                    format!("Path not found: {}", path_for_line_output(&dir_path)),
                )
            } else {
                Error::tool(
                    "ls",
                    format!("Cannot read directory: {}", error_for_line_output(&err)),
                )
            }
        })?;
        if !dir_metadata.is_dir() {
            return Err(Error::tool(
                "ls",
                format!("Not a directory: {}", path_for_line_output(&dir_path)),
            ));
        }
        ensure_ancestors_searchable(&dir_path)
            .await
            .map_err(|err| {
                Error::tool(
                    "ls",
                    format!("Cannot read directory: {}", error_for_line_output(&err)),
                )
            })?;
        ensure_effective_mode_access(
            &dir_metadata,
            &dir_path,
            UNIX_ACCESS_READ | UNIX_ACCESS_SEARCH,
            "directory listing",
        )
        .map_err(|err| {
            Error::tool(
                "ls",
                format!("Cannot read directory: {}", error_for_line_output(&err)),
            )
        })?;
        #[cfg(test)]
        let after_scope_hook = self.after_scope_hook.clone();
        #[cfg(not(test))]
        let after_scope_hook = None;
        let scoped_root = open_scoped_scan_root(&dir_path, &self.cwd, false, after_scope_hook)
            .await
            .map_err(|err| {
                Error::tool(
                    "ls",
                    format!(
                        "Cannot pin directory {} for listing: {}",
                        path_for_line_output(&dir_path),
                        error_for_line_output(&err)
                    ),
                )
            })?;
        let listing_path = scoped_root.io_path();

        let cache_key = tool_cache_key("ls", &self.cwd, &input_value);
        let cache_mode = ToolCacheFingerprintMode::DirectoryImmediate;
        let cache_deps =
            cache_dependencies_for_scoped_scan(&scoped_root, &cwd_scope, cache_mode, None);
        if let Some(output) = cached_tool_output(&cache_key, cache_deps.as_deref()) {
            return Ok(output);
        }

        let mut entries = Vec::new();
        let mut read_dir = asupersync::fs::read_dir(&listing_path).await.map_err(|e| {
            Error::tool(
                "ls",
                format!("Cannot read directory: {}", error_for_line_output(&e)),
            )
        })?;

        let mut scan_limit_reached = false;
        while let Some(entry) = read_dir.next_entry().await.map_err(|e| {
            Error::tool(
                "ls",
                format!("Cannot read directory entry: {}", error_for_line_output(&e)),
            )
        })? {
            if entries.len() >= LS_SCAN_HARD_LIMIT {
                scan_limit_reached = true;
                break;
            }
            let name = entry.file_name();
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

        // Sort alphabetically (case-insensitive) with a lossless deterministic
        // tie-breaker for names that differ only by case or invalid Unix bytes.
        entries.sort_by(|(a, _), (b, _)| compare_paths_for_line_output(Path::new(a), Path::new(b)));

        let mut output_builder = HeadTruncatingLineWriter::new(DEFAULT_MAX_BYTES);
        let mut artifact_source = String::new();
        let mut emitted_entries = 0usize;
        let mut entry_limit_reached = false;

        for (entry, is_dir) in entries {
            if emitted_entries >= effective_limit {
                entry_limit_reached = true;
                break;
            }
            let escaped_entry = path_for_line_output(Path::new(&entry));
            let line = if is_dir {
                format!("{escaped_entry}/")
            } else {
                escaped_entry
            };
            output_builder.push_line(&line);
            append_artifact_source_line(&mut artifact_source, &line);
            emitted_entries = emitted_entries.saturating_add(1);
        }

        if emitted_entries == 0 {
            let output = ToolOutput {
                content: vec![ContentBlock::Text(TextContent::new("(empty directory)"))],
                details: None,
                is_error: false,
            };
            cache_tool_output(
                cache_key,
                stable_cache_dependencies_for_scoped_scan(
                    &scoped_root,
                    &cwd_scope,
                    cache_mode,
                    None,
                    cache_deps.as_deref(),
                ),
                &output,
            );
            return Ok(output);
        }

        // Apply byte truncation while writing, avoiding a second joined copy.
        let mut truncation = output_builder.finish();

        let mut output = std::mem::take(&mut truncation.content);
        let mut details_map = serde_json::Map::new();
        let mut notices: Vec<String> = Vec::new();

        if entry_limit_reached {
            notices.push(format!(
                "{effective_limit} entries limit reached. Use limit={} for more",
                effective_limit.saturating_mul(2)
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

        let mut details = if details_map.is_empty() {
            None
        } else {
            Some(serde_json::Value::Object(details_map))
        };

        attach_text_artifact_if_needed_with_root(
            self.artifact_root.as_deref(),
            &mut output,
            &mut details,
            "ls",
            tool_call_id,
            "directoryEntries",
            &artifact_source,
        );

        let output = ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(output))],
            details,
            is_error: false,
        };
        cache_tool_output(
            cache_key,
            stable_cache_dependencies_for_scoped_scan(
                &scoped_root,
                &cwd_scope,
                cache_mode,
                None,
                cache_deps.as_deref(),
            ),
            &output,
        );
        Ok(output)
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
                && let Ok(metadata) = entry.metadata()
                && metadata.modified().is_ok_and(|modified| {
                    modified
                        .elapsed()
                        .is_ok_and(|age| age > Duration::from_hours(24))
                })
                && let Err(e) = std::fs::remove_file(&path)
            {
                // Log but don't panic on cleanup failure
                tracing::debug!("Failed to remove temp file {}: {}", path.display(), e);
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
            Err(err) if matches!(err.kind(), std::io::ErrorKind::Interrupted) => {}
            Err(err) => return Err(err.to_string()),
        }
    }

    Ok(captured)
}

// Keep `rx` as `&mut Receiver`: `std::sync::mpsc::Receiver` is `Send` but not
// `Sync`, and this helper awaits between polls, so `&Receiver` would make the
// surrounding future non-Send.
#[allow(clippy::needless_pass_by_ref_mut)]
#[cfg(test)]
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
        if let Some(path) = self.temp_file_path.take()
            && let Err(e) = std::fs::remove_file(&path)
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
                        // asupersync 0.3.6's fs::Metadata no longer exposes the
                        // inode (and fs::File has no general AsRawFd), so re-stat
                        // the path with std symlink_metadata (does not follow
                        // symlinks) for the TOCTOU/identity guard.
                        match std::fs::symlink_metadata(&path) {
                            Ok(meta) => {
                                if !meta.ino().eq(&expected) {
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

        if let Some(timeout) = state.timeout_ms
            && let Some(progress) = details_map
                .get_mut("progress")
                .and_then(|v| v.as_object_mut())
        {
            progress.insert("timeoutMs".into(), serde_json::json!(timeout));
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

/// Attach a freshly-spawned child to platform tree-discipline bookkeeping.
///
/// Windows assigns the child to a kill-on-close Job object so later
/// `kill_process_group_tree` / `terminate_process_group_tree` calls reap the
/// whole descendant tree, including processes spawned between the kill-time
/// snapshot and the kill (bd-9jgrt item 1). Unix needs nothing here: the
/// child already leads its own process group. Hub PTY services are out of
/// scope (portable-pty children keep the walk-based discipline).
// Const only on unix where the body degenerates to `let _`; the windows
// branch calls non-const job registration, so the lint cannot hold for both
// targets at once.
#[allow(clippy::missing_const_for_fn)]
pub(crate) fn attach_child_job_discipline(child: &std::process::Child) {
    #[cfg(windows)]
    win_job::attach(child);
    #[cfg(not(windows))]
    {
        let _ = child;
    }
}

#[cfg(windows)]
mod win_job {
    //! Kill-on-close Job objects keyed by root pid (bd-9jgrt item 1).
    //!
    //! Windows has no process groups, so the parent-chain walk in
    //! [`kill_process_tree_with`] races grandchildren spawned after its
    //! process snapshot. A job closes that race: descendants cannot break
    //! away (`JOB_OBJECT_LIMIT_BREAKAWAY_OK` is never set), and dropping the
    //! job's handle terminates every current member in one shot.

    use std::collections::HashMap;
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;
    use std::sync::{LazyLock, Mutex};

    use win32job::{ExtendedLimitInfo, Job};

    static REGISTRY: LazyLock<Mutex<HashMap<u32, Job>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));

    /// Assign `child` to a fresh kill-on-close job and remember it by pid.
    ///
    /// Best-effort: any failure leaves the walk-based discipline below in
    /// charge (e.g. systems that reject nested-job assignment fall back).
    pub(crate) fn attach(child: &Child) {
        let Ok(mut map) = REGISTRY.lock() else {
            return;
        };
        prune_dead_entries(&mut map);
        let mut info = ExtendedLimitInfo::new();
        info.limit_kill_on_job_close();
        let Ok(job) = Job::create_with_limit_info(&info) else {
            return;
        };
        // RawHandle is *mut c_void; win32job takes the isize numeric handle.
        if job.assign_process(child.as_raw_handle() as isize).is_err() {
            // Never assigned, so closing harms nothing.
            drop(job);
            return;
        }
        map.insert(child.id(), job);
    }

    /// Kill the tree rooted at `pid` via its job, returning whether one
    /// existed. Dropping the stored `Job` closes the handle, and
    /// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` does the actual termination.
    pub(crate) fn terminate(pid: u32) -> bool {
        REGISTRY
            .lock()
            .ok()
            .and_then(|mut map| map.remove(&pid))
            .is_some()
    }

    /// Drop entries whose root process no longer exists so the map (and its
    /// kernel handles) stays proportional to live spawned children. Skipped
    /// while small to avoid a process-table refresh per trivial spawn.
    fn prune_dead_entries(map: &mut HashMap<u32, Job>) {
        if map.len() < 8 {
            return;
        }
        let pids: Vec<sysinfo::Pid> = map.keys().map(|&pid| sysinfo::Pid::from_u32(pid)).collect();
        let mut sys = sysinfo::System::new();
        sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&pids), true);
        let dead: Vec<u32> = pids
            .iter()
            .filter(|pid| sys.process(**pid).is_none())
            .map(|pid| pid.as_u32())
            .collect();
        for pid in dead {
            // Jobs whose root died are empty; dropping just reclaims the handle.
            map.remove(&pid);
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

pub fn kill_process_group_tree(pid: Option<u32>) {
    kill_process_tree_with(pid, sysinfo::Signal::Kill, true);
}

pub(crate) fn terminate_process_group_tree(pid: Option<u32>) {
    kill_process_tree_with(pid, sysinfo::Signal::Term, true);
}

fn kill_process_tree_with(pid: Option<u32>, signal: sysinfo::Signal, include_process_group: bool) {
    let Some(pid) = pid else {
        return;
    };

    // Windows fast path: children spawned through
    // `attach_child_job_discipline` carry a Job object containing their whole
    // descendant tree, so dropping the job kills everyone in one shot —
    // including grandchildren that appeared after this function would
    // otherwise have taken its snapshot (bd-9jgrt item 1). Pids without a
    // job (hub PTY services, external pids) fall through to the walk below.
    #[cfg(windows)]
    if win_job::terminate(pid) {
        return;
    }

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
    let command = Command::new(program); // ubs:ignore policy-checked non-Unix command runner
    Ok(command)
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
            Err(err) if matches!(err.kind(), std::io::ErrorKind::NotFound) => {}
            Err(err) if matches!(err.kind(), std::io::ErrorKind::PermissionDenied) => {
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
        let rel_str = path_for_line_output(rel);
        if !rel_str.is_empty() {
            return rel_str;
        }
    }

    let canonical_file = safe_canonicalize(file_path);
    let canonical_cwd = safe_canonicalize(cwd);
    if let Ok(rel) = canonical_file.strip_prefix(&canonical_cwd) {
        let rel_str = path_for_line_output(rel);
        if !rel_str.is_empty() {
            return rel_str;
        }
    }

    path_for_line_output(file_path)
}

async fn get_file_lines_async(path: &Path, cwd: &Path) -> Vec<String> {
    // Match Node's `readFileSync(..., "utf-8")` behavior: decode lossily rather than failing.
    // Open the exact result path without following a terminal link and
    // verify its descriptor identity against the scoped path snapshot.
    // This prevents a concurrent workspace mutation from swapping an rg
    // result to an out-of-scope file between search and context rendering.
    let path_for_read = path.to_path_buf();
    let cwd_for_read = cwd.to_path_buf();
    let bytes = match asupersync::runtime::spawn_blocking_io(move || {
        read_scoped_file_capped_sync(&path_for_read, &cwd_for_read, GREP_CONTEXT_MAX_FILE_BYTES)
    })
    .await
    {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::debug!(
                "Failed to read grep file {}: {}",
                path_for_line_output(path),
                error_for_line_output(&err)
            );
            return Vec::new();
        }
    };
    split_grep_context_lines(&bytes, path)
}

/// Render grep context lines for a pinned regular-file scan root by reading
/// the held descriptor itself (offset-independent `read_at`/`seek_read`), so
/// a rename/symlink swap of the logical path after pinning cannot redirect or
/// break context rendering.
async fn get_pinned_file_lines_async(scoped_root: &ScopedScanRoot) -> Vec<String> {
    let logical = scoped_root.logical_path().to_path_buf();
    #[cfg(unix)]
    let handle = match scoped_root.handle.try_clone() {
        Ok(handle) => handle,
        Err(err) => {
            tracing::debug!(
                "Failed to clone pinned grep file handle {}: {}",
                path_for_line_output(&logical),
                error_for_line_output(&err)
            );
            return Vec::new();
        }
    };
    #[cfg(unix)]
    let bytes = {
        let read = asupersync::runtime::spawn_blocking_io(move || {
            use std::os::unix::fs::FileExt as _;

            let cap = usize::try_from(GREP_CONTEXT_MAX_FILE_BYTES).unwrap_or(usize::MAX);
            let mut bytes = Vec::new();
            let mut offset = 0u64;
            // Heap-allocated: 64KiB on the stack trips clippy::large_stack_arrays.
            let mut chunk = vec![0u8; 64 * 1024];
            loop {
                let read = handle.read_at(&mut chunk, offset)?;
                if read == 0 {
                    break;
                }
                bytes.extend_from_slice(&chunk[..read]);
                offset = offset.saturating_add(read as u64);
                if bytes.len() > cap {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("pinned grep file exceeds {GREP_CONTEXT_MAX_FILE_BYTES} bytes"),
                    ));
                }
            }
            Ok(bytes)
        })
        .await;
        match read {
            Ok(bytes) => bytes,
            Err(err) => {
                tracing::debug!(
                    "Failed to read pinned grep file {}: {}",
                    path_for_line_output(&logical),
                    error_for_line_output(&err)
                );
                return Vec::new();
            }
        }
    };
    // Windows pins parent components rather than the file itself; the
    // logical-path read below matches the platform's established posture.
    #[cfg(not(unix))]
    let bytes = {
        let path_for_read = logical.clone();
        match asupersync::runtime::spawn_blocking_io(move || {
            read_file_capped_sync(&path_for_read, GREP_CONTEXT_MAX_FILE_BYTES)
        })
        .await
        {
            Ok(bytes) => bytes,
            Err(err) => {
                tracing::debug!(
                    "Failed to read pinned grep file {}: {}",
                    path_for_line_output(&logical),
                    error_for_line_output(&err)
                );
                return Vec::new();
            }
        }
    };
    split_grep_context_lines(&bytes, &logical)
}

/// Lossily decode grep file bytes into display lines, normalizing `\r`
/// variants and enforcing [`GREP_CONTEXT_MAX_LINES`].
fn split_grep_context_lines(bytes: &[u8], path: &Path) -> Vec<String> {
    let content = String::from_utf8_lossy(bytes);
    let mut lines = Vec::new();
    for line in content.split('\n') {
        let trimmed = line.strip_suffix('\r').unwrap_or(line);
        for piece in trimmed.split('\r') {
            if lines.len() == GREP_CONTEXT_MAX_LINES {
                tracing::debug!(
                    "Refusing to materialize more than {GREP_CONTEXT_MAX_LINES} grep context lines from {}",
                    path_for_line_output(path)
                );
                return Vec::new();
            }
            lines.push(piece.to_string());
        }
    }
    if content.ends_with('\n') && lines.last().is_some_and(std::string::String::is_empty) {
        lines.pop();
    }
    lines
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
    before_persist_hook: Option<Arc<dyn Fn() + Send + Sync>>,
    mutation_recorder: Option<Arc<crate::undo::FileMutationRecorder>>,
    workspace: WorkspaceHandle,
}

impl HashlineEditTool {
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            before_persist_hook: None,
            mutation_recorder: None,
            workspace: WorkspaceHandle::default(),
        }
    }

    /// Attach the session's undo recorder (bd-cv653.3.13).
    #[must_use]
    pub fn with_mutation_recorder(
        mut self,
        recorder: Option<Arc<crate::undo::FileMutationRecorder>>,
    ) -> Self {
        self.mutation_recorder = recorder;
        self
    }

    /// Attach the session workspace root handle (bd-cv653.3.12).
    #[must_use]
    pub fn with_workspace(mut self, workspace: WorkspaceHandle) -> Self {
        self.workspace = workspace;
        self
    }

    #[cfg(test)]
    fn with_before_persist_hook(cwd: &Path, hook: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            before_persist_hook: Some(Arc::new(hook)),
            mutation_recorder: None,
            workspace: WorkspaceHandle::default(),
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
        if let Some(ref pos) = edit.pos
            && let Err(e) = validate_line_ref(pos, file_lines, had_bom)
        {
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
        if let Some(ref end) = edit.end
            && let Err(e) = validate_line_ref(end, file_lines, had_bom)
        {
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
        tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let input: HashlineEditInput = serde_json::from_value(input)
            .map_err(|e| Error::tool("hashline_edit", format!("Invalid input: {e}")))?;

        if input.edits.is_empty() {
            return Err(Error::tool("hashline_edit", "No edits provided"));
        }

        // Scheme URLs route through the router — never a silent filesystem
        // fallback (bd-cv653.6.5).
        if crate::url_router::has_scheme(&input.path) {
            return self.execute_ssh_hashline_edit(&input).await;
        }

        // Resolve file path and enforce scope before touching the filesystem.
        let resolved = resolve_read_path(&input.path, &self.cwd);
        let absolute_path =
            enforce_cwd_scope(&resolved, &self.cwd, "hashline_edit", &self.workspace)?;

        // Check file size
        let metadata = std_metadata_async(&absolute_path).await.map_err(|err| {
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
        ensure_ancestors_searchable(&absolute_path)
            .await
            .map_err(|err| Error::tool("hashline_edit", err.to_string()))?;
        ensure_effective_mode_access(
            &metadata,
            &absolute_path,
            UNIX_ACCESS_READ | UNIX_ACCESS_WRITE,
            "hashline editing",
        )
        .map_err(|err| Error::tool("hashline_edit", err.to_string()))?;
        ensure_parent_allows_creation(&absolute_path)
            .await
            .map_err(|err| Error::tool("hashline_edit", err.to_string()))?;
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

        // Read the transformation source through the validated, no-follow
        // descriptor path. The digest of these exact bytes becomes the
        // optimistic-CAS expectation checked immediately before rename.
        let path_for_read = absolute_path.clone();
        let cwd_for_read = self.cwd.clone();
        let raw = asupersync::runtime::spawn_blocking_io(move || {
            read_scoped_file_capped_sync(&path_for_read, &cwd_for_read, READ_TOOL_MAX_BYTES)
        })
        .await
        .map_err(|e| Error::tool("hashline_edit", format!("Cannot read file: {e}")))?;

        if raw.len() as u64 > READ_TOOL_MAX_BYTES {
            return Err(Error::tool(
                "hashline_edit",
                format!("File too large (> {READ_TOOL_MAX_BYTES} bytes)"),
            ));
        }

        let source_expectation = AtomicContentExpectation::from_bytes(&raw);
        let raw_content = String::from_utf8(raw).map_err(|_| {
            Error::tool(
                "hashline_edit",
                "File contains invalid UTF-8 characters and cannot be safely edited as text."
                    .to_string(),
            )
        })?;

        let outcome = apply_hashline_edits_to_content(&raw_content, &input.edits, &input.path)?;
        let final_content = outcome.final_content;
        let normalized = outcome.before_normalized;
        let new_normalized = outcome.after_normalized;

        // Atomic write (same pattern as EditTool)
        let absolute_path_clone = absolute_path.clone();
        // Full canonical root set (primary + /add-dir roots) so mutations in
        // additional workspace roots are not rejected by the atomic-replace
        // containment check.
        let cwd_clone = self.workspace.snapshot_or(&self.cwd).all();
        let final_content_bytes = final_content.into_bytes();
        let before_persist_hook = self.before_persist_hook.clone();
        if let Some(recorder) = &self.mutation_recorder {
            recorder.begin_file(tool_call_id, "hashline_edit", &absolute_path);
        }
        let persisted = asupersync::runtime::spawn_blocking_io(move || {
            before_persist_hook.map_or_else(
                || {
                    atomic_replace_file_if_unchanged(
                        &absolute_path_clone,
                        &cwd_clone,
                        &final_content_bytes,
                        source_expectation,
                    )
                },
                |hook| {
                    atomic_replace_file_with(
                        &absolute_path_clone,
                        &cwd_clone,
                        &final_content_bytes,
                        Some(source_expectation),
                        move || hook(),
                    )
                },
            )
        })
        .await
        .map_err(|e| Error::tool("hashline_edit", format!("Failed to write file: {e}")));
        if let Some(recorder) = &self.mutation_recorder {
            if persisted.is_ok() {
                recorder.commit(tool_call_id);
            } else {
                recorder.abort(tool_call_id);
            }
        }
        persisted?;

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

    #[cfg(unix)]
    struct UnixModeGuard {
        path: PathBuf,
        original: std::fs::Permissions,
    }

    #[cfg(unix)]
    impl UnixModeGuard {
        fn set(path: &Path, mode: u32) -> Self {
            use std::os::unix::fs::PermissionsExt as _;

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

    #[test]
    fn fsync_refusal_classifies_non_posix_durability_errors() {
        use std::io::{Error, ErrorKind};

        // EBADF (the exact errno reported against virtiofs in issue #136) and
        // EINVAL (common for directory fsync on FUSE/network mounts) are
        // filesystem *refusals* of the durability barrier, not write failures.
        assert!(is_fsync_refused(&Error::from_raw_os_error(9))); // EBADF
        assert!(is_fsync_refused(&Error::from_raw_os_error(22))); // EINVAL
        assert!(is_fsync_refused(&Error::new(
            ErrorKind::Unsupported,
            "nope"
        )));

        // Genuine I/O failures must still propagate so real corruption/space
        // problems are never silently swallowed.
        assert!(!is_fsync_refused(&Error::from_raw_os_error(5))); // EIO
        assert!(!is_fsync_refused(&Error::from_raw_os_error(28))); // ENOSPC
        assert!(!is_fsync_refused(&Error::new(
            ErrorKind::PermissionDenied,
            "no"
        )));
    }

    #[test]
    fn tolerate_fsync_refusal_downgrades_refusals_but_propagates_real_errors() {
        use std::io::{Error, ErrorKind};
        use std::path::Path;

        let p = Path::new("/tmp/does-not-matter");
        // Refusals are downgraded to Ok so an already-written file is not
        // reported as a failed write.
        assert!(tolerate_fsync_refusal(Ok(()), "x", p).is_ok());
        assert!(tolerate_fsync_refusal(Err(Error::from_raw_os_error(9)), "temp file", p).is_ok());
        // Real I/O errors still surface to the caller.
        assert!(tolerate_fsync_refusal(Err(Error::from_raw_os_error(5)), "temp file", p).is_err());
        assert!(
            tolerate_fsync_refusal(Err(Error::new(ErrorKind::PermissionDenied, "no")), "x", p)
                .is_err()
        );
    }

    #[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
    #[test]
    fn atomic_replace_parent_swap_cannot_redirect_persistence_outside_cwd() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().expect("atomic replacement fixture");
        let cwd = tmp.path().join("workspace");
        let parent = cwd.join("parent");
        std::fs::create_dir_all(&parent).expect("create replacement parent");
        let target = parent.join("target.txt");
        std::fs::write(&target, "original\n").expect("write original target");

        let outside_parent = tmp.path().join("outside");
        std::fs::create_dir(&outside_parent).expect("create outside parent");
        let outside_target = outside_parent.join("target.txt");
        std::fs::write(&outside_target, "outside sentinel\n").expect("write outside sentinel");
        let displaced_parent = cwd.join("parent-original");

        let result = atomic_replace_file_with(
            &target,
            std::slice::from_ref(&cwd),
            b"replacement\n",
            None,
            || {
                std::fs::rename(&parent, &displaced_parent).expect("displace validated parent");
                symlink(&outside_parent, &parent).expect("redirect parent path outside cwd");
            },
        );

        let error = result.expect_err("a changed parent pathname must be reported");
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(
            std::fs::read_to_string(&outside_target).expect("read outside sentinel"),
            "outside sentinel\n",
            "descriptor-relative rename must not touch the redirected target"
        );
        assert_eq!(
            std::fs::read_to_string(displaced_parent.join("target.txt"))
                .expect("read unchanged pinned-directory target"),
            "original\n",
            "a parent swap observed before rename must abort the mutation"
        );
    }

    #[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
    #[test]
    fn edit_rejects_same_inode_change_before_persist_and_preserves_concurrent_writer() {
        use std::os::unix::fs::MetadataExt as _;

        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().expect("edit CAS fixture");
            let target = tmp.path().join("target.txt");
            std::fs::write(&target, "before\nORIGINAL\nafter\n").expect("write edit source");
            let original_inode = std::fs::metadata(&target).expect("stat edit source").ino();
            let hook_target = target.clone();
            let tool = EditTool::with_before_persist_hook(tmp.path(), move || {
                std::fs::write(&hook_target, "concurrent writer\n")
                    .expect("same-inode concurrent edit");
                assert_eq!(
                    std::fs::metadata(&hook_target)
                        .expect("stat concurrent edit")
                        .ino(),
                    original_inode,
                    "fixture mutation must preserve the inode"
                );
            });

            let error = tool
                .execute(
                    "edit-cas",
                    serde_json::json!({
                        "path": target,
                        "oldText": "ORIGINAL",
                        "newText": "replacement"
                    }),
                    None,
                )
                .await
                .expect_err("Edit must reject a same-inode concurrent change");

            assert!(
                error.to_string().contains("changed since it was read"),
                "unexpected Edit conflict: {error}"
            );
            assert_eq!(
                std::fs::read_to_string(&target).expect("read concurrent Edit result"),
                "concurrent writer\n",
                "Edit must not replace the concurrent writer"
            );
            assert_eq!(
                std::fs::metadata(&target)
                    .expect("stat preserved concurrent Edit result")
                    .ino(),
                original_inode
            );
        });
    }

    #[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
    #[test]
    fn hashline_edit_rejects_same_inode_change_and_preserves_concurrent_writer() {
        use std::os::unix::fs::MetadataExt as _;

        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().expect("hashline CAS fixture");
            let target = tmp.path().join("target.txt");
            std::fs::write(&target, "before\nORIGINAL\nafter\n").expect("write hashline source");
            let original_inode = std::fs::metadata(&target)
                .expect("stat hashline source")
                .ino();
            let original_tag = format_hashline_tag(1, "ORIGINAL");
            let hook_target = target.clone();
            let tool = HashlineEditTool::with_before_persist_hook(tmp.path(), move || {
                std::fs::write(&hook_target, "concurrent hashline writer\n")
                    .expect("same-inode concurrent hashline edit");
                assert_eq!(
                    std::fs::metadata(&hook_target)
                        .expect("stat concurrent hashline edit")
                        .ino(),
                    original_inode,
                    "fixture mutation must preserve the inode"
                );
            });

            let error = tool
                .execute(
                    "hashline-cas",
                    serde_json::json!({
                        "path": target,
                        "edits": [{
                            "op": "replace",
                            "pos": original_tag,
                            "lines": "replacement"
                        }]
                    }),
                    None,
                )
                .await
                .expect_err("HashlineEdit must reject a same-inode concurrent change");

            assert!(
                error.to_string().contains("changed since it was read"),
                "unexpected HashlineEdit conflict: {error}"
            );
            assert_eq!(
                std::fs::read_to_string(&target).expect("read concurrent HashlineEdit result"),
                "concurrent hashline writer\n",
                "HashlineEdit must not replace the concurrent writer"
            );
            assert_eq!(
                std::fs::metadata(&target)
                    .expect("stat preserved concurrent HashlineEdit result")
                    .ino(),
                original_inode
            );
        });
    }

    #[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
    #[test]
    fn write_intentionally_overwrites_same_inode_change_before_persist() {
        use std::os::unix::fs::MetadataExt as _;

        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().expect("write overwrite fixture");
            let target = tmp.path().join("target.txt");
            std::fs::write(&target, "original\n").expect("write initial target");
            let original_inode = std::fs::metadata(&target)
                .expect("stat initial target")
                .ino();
            let hook_target = target.clone();
            let tool = WriteTool::with_before_persist_hook(tmp.path(), move || {
                std::fs::write(&hook_target, "concurrent writer\n")
                    .expect("same-inode concurrent write");
                assert_eq!(
                    std::fs::metadata(&hook_target)
                        .expect("stat concurrent write")
                        .ino(),
                    original_inode,
                    "fixture mutation must preserve the inode"
                );
            });

            tool.execute(
                "write-overwrite",
                serde_json::json!({
                    "path": target,
                    "content": "requested write\n"
                }),
                None,
            )
            .await
            .expect("Write intentionally overwrites concurrent content");

            assert_eq!(
                std::fs::read_to_string(&target).expect("read final Write result"),
                "requested write\n"
            );
        });
    }

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

    fn assert_same_head_truncation(actual: &TruncationResult, expected: &TruncationResult) {
        assert_eq!(actual.content, expected.content);
        assert_eq!(actual.truncated, expected.truncated);
        assert_eq!(actual.truncated_by, expected.truncated_by);
        assert_eq!(actual.total_lines, expected.total_lines);
        assert_eq!(actual.total_bytes, expected.total_bytes);
        assert_eq!(actual.output_lines, expected.output_lines);
        assert_eq!(actual.output_bytes, expected.output_bytes);
        assert_eq!(actual.last_line_partial, expected.last_line_partial);
        assert_eq!(
            actual.first_line_exceeds_limit,
            expected.first_line_exceeds_limit
        );
        assert_eq!(actual.max_lines, expected.max_lines);
        assert_eq!(actual.max_bytes, expected.max_bytes);
    }

    fn write_lines_with_builder(lines: &[&str], max_bytes: usize) -> TruncationResult {
        let mut writer = HeadTruncatingLineWriter::new(max_bytes);
        for line in lines {
            writer.push_line(line);
        }
        writer.finish()
    }

    #[test]
    fn head_truncating_line_writer_matches_join_without_truncation() {
        let lines = ["alpha", "beta", "gamma"];
        let expected = truncate_head(lines.join("\n"), usize::MAX, 1000);
        let actual = write_lines_with_builder(&lines, 1000);

        assert_same_head_truncation(&actual, &expected);
    }

    #[test]
    fn head_truncating_line_writer_matches_join_at_byte_boundary() {
        let lines = ["alpha", "beta", "gamma"];
        let expected = truncate_head(lines.join("\n"), usize::MAX, 8);
        let actual = write_lines_with_builder(&lines, 8);

        assert_same_head_truncation(&actual, &expected);
        assert_eq!(actual.content, "alpha\nbe");
    }

    #[test]
    fn head_truncating_line_writer_preserves_utf8_boundary_and_order() {
        let lines = ["alpha", "βeta", "gamma"];
        let expected = truncate_head(lines.join("\n"), usize::MAX, 8);
        let actual = write_lines_with_builder(&lines, 8);

        assert_same_head_truncation(&actual, &expected);
        assert_eq!(actual.content, "alpha\nβ");
    }

    fn first_text(output: &ToolOutput) -> &str {
        output
            .content
            .first()
            .and_then(|block| match block {
                ContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .unwrap_or("")
    }

    fn artifact_json(details: Option<&serde_json::Value>) -> &serde_json::Value {
        details
            .and_then(|value| value.get("artifact"))
            .expect("artifact details")
    }

    fn artifact_str_field<'a>(artifact: &'a serde_json::Value, field: &str) -> &'a str {
        artifact
            .get(field)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
    }

    #[test]
    fn tool_output_artifact_respects_spill_threshold() {
        let tmp = tempfile::tempdir().expect("artifact root");
        let mut output = "small preview".to_string();
        let mut details = None;
        let spilled = attach_text_artifact_if_needed_at_root(
            tmp.path(),
            &mut output,
            &mut details,
            "read",
            "call-small",
            "selectedTextWindow",
            "small body",
        );

        assert!(!spilled);
        assert_eq!(output, "small preview");
        assert!(details.is_none());
    }

    #[test]
    fn tool_output_artifact_writes_content_addressed_text_and_metadata()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir().expect("artifact root");
        let full = "a".repeat(TOOL_OUTPUT_ARTIFACT_THRESHOLD_BYTES + 1);
        let mut output = "bounded preview".to_string();
        let mut details = None;
        let _session_guard =
            register_tool_output_artifact_session("call/text:1", "session/artifacts:one");
        let spilled = attach_text_artifact_if_needed_at_root(
            tmp.path(),
            &mut output,
            &mut details,
            "read",
            "call/text:1",
            "selectedTextWindow",
            &full,
        );

        assert!(spilled);
        assert!(output.contains("Full tool output artifact:"));
        let artifact = artifact_json(details.as_ref());
        assert_eq!(artifact["schema"], TOOL_OUTPUT_ARTIFACT_SCHEMA_V1);
        assert_eq!(artifact["toolName"], "read");
        assert_eq!(artifact["sourceKind"], "selectedTextWindow");
        assert_eq!(artifact["sessionId"], "session/artifacts:one");
        assert_eq!(
            artifact["byteCount"].as_u64().unwrap(),
            u64::try_from(full.len()).unwrap()
        );

        let path_value = artifact_str_field(artifact, "path");
        let metadata_path_value = artifact_str_field(artifact, "metadataPath");
        assert!(!path_value.is_empty(), "artifact path must be a string");
        assert!(
            !metadata_path_value.is_empty(),
            "artifact metadataPath must be a string"
        );
        let path = PathBuf::from(path_value);
        let metadata_path = PathBuf::from(metadata_path_value);
        assert!(path.starts_with(tmp.path().join("session_artifacts_one").join("call_text_1")));
        assert_eq!(std::fs::read_to_string(path)?, full);
        let metadata_bytes = std::fs::read(metadata_path)?;
        let metadata: serde_json::Value = serde_json::from_slice(&metadata_bytes)?;
        assert_eq!(metadata["sha256"], artifact["sha256"]);
        assert_eq!(
            metadata["retentionClass"],
            TOOL_OUTPUT_ARTIFACT_RETENTION_CLASS
        );
        assert_eq!(
            metadata["spilloverReason"],
            TOOL_OUTPUT_ARTIFACT_SPILLOVER_REASON
        );
        assert_eq!(metadata["safeDeleteCandidate"], true);
        assert_eq!(
            metadata["redactionSummary"]["policy"],
            TOOL_OUTPUT_ARTIFACT_REDACTION_POLICY_V1
        );
        assert_eq!(metadata["redactionSummary"]["status"], "clean");
        assert_eq!(metadata["redactionSummary"]["rawSecretBytesEmitted"], 0);
        Ok(())
    }

    #[test]
    fn tool_output_artifact_redacts_sensitive_text_before_persisting()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir().expect("artifact root");
        let leaked_token = "sk-redactionfixture1234567890";
        let leaked_bearer = "ghp_redactionfixture1234567890";
        let full = format!(
            "API_TOKEN={leaked_token}\nAuthorization: Bearer {leaked_bearer}\n{}",
            "x".repeat(TOOL_OUTPUT_ARTIFACT_THRESHOLD_BYTES + 1)
        );
        let mut output = "bounded preview".to_string();
        let mut details = None;

        let spilled = attach_text_artifact_if_needed_at_root(
            tmp.path(),
            &mut output,
            &mut details,
            "read",
            "call-secret",
            "selectedTextWindow",
            &full,
        );

        assert!(spilled);
        let artifact = artifact_json(details.as_ref());
        let path = PathBuf::from(artifact_str_field(artifact, "path"));
        let metadata_path = PathBuf::from(artifact_str_field(artifact, "metadataPath"));
        let persisted = std::fs::read_to_string(path)?;
        let metadata: serde_json::Value = serde_json::from_slice(&std::fs::read(metadata_path)?)?;

        assert!(!persisted.contains(leaked_token));
        assert!(!persisted.contains(leaked_bearer));
        assert!(persisted.contains("API_TOKEN=[REDACTED]"));
        assert_eq!(artifact["redactionSummary"]["status"], "redacted");
        assert_eq!(artifact["redactionSummary"]["rawSecretBytesEmitted"], 0);
        assert_eq!(metadata["redactionSummary"], artifact["redactionSummary"]);
        let fields = artifact["redactionSummary"]["fields"]
            .as_array()
            .expect("redaction fields");
        assert!(fields.iter().any(|field| field == "api_token"));
        assert!(fields.iter().any(|field| field == "authorization"));
        Ok(())
    }

    #[test]
    fn tool_output_artifact_marks_binaryish_payloads_in_lifecycle_manifest() {
        let tmp = tempfile::tempdir().expect("artifact root");
        let full = format!(
            "{}\0{}",
            "z".repeat(TOOL_OUTPUT_ARTIFACT_THRESHOLD_BYTES / 2),
            "z".repeat(TOOL_OUTPUT_ARTIFACT_THRESHOLD_BYTES / 2 + 2)
        );
        let mut output = "bounded preview".to_string();
        let mut details = None;

        let spilled = attach_text_artifact_if_needed_at_root(
            tmp.path(),
            &mut output,
            &mut details,
            "read",
            "call-binaryish",
            "selectedTextWindow",
            &full,
        );

        assert!(spilled);
        let artifact = artifact_json(details.as_ref());
        assert_eq!(artifact["redactionSummary"]["binarySuspect"], true);
        assert_eq!(artifact["redactionSummary"]["rawSecretBytesEmitted"], 0);
        assert_eq!(artifact["safeDeleteCandidate"], true);
    }

    #[test]
    fn tool_output_artifact_failure_records_degraded_preview() {
        let tmp = tempfile::tempdir().expect("artifact root parent");
        let root_file = tmp.path().join("not-a-directory");
        std::fs::write(&root_file, "not a directory").expect("root file");
        let full = "b".repeat(TOOL_OUTPUT_ARTIFACT_THRESHOLD_BYTES + 1);
        let mut output = "bounded preview".to_string();
        let mut details = None;

        let spilled = attach_text_artifact_if_needed_at_root(
            &root_file,
            &mut output,
            &mut details,
            "read",
            "call-fail",
            "selectedTextWindow",
            &full,
        );

        assert!(!spilled);
        assert!(output.contains("Tool output artifact persistence failed"));
        assert!(
            details
                .as_ref()
                .and_then(|value| value.get("artifactError"))
                .is_some()
        );
    }

    #[test]
    fn read_tool_spills_oversized_selected_text_window_to_artifact() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().expect("workspace");
            let artifact_root = tempfile::tempdir().expect("artifact root");

            let body = "r".repeat(TOOL_OUTPUT_ARTIFACT_THRESHOLD_BYTES + 8);
            std::fs::write(tmp.path().join("large.txt"), &body).expect("large file");
            let read_tool = ReadTool::with_artifact_root(tmp.path(), artifact_root.path());
            let output = read_tool
                .execute(
                    "read-artifact-call",
                    serde_json::json!({ "path": "large.txt" }),
                    None,
                )
                .await
                .expect("read large file");

            assert!(first_text(&output).contains("Full tool output artifact:"));
            let artifact = artifact_json(output.details.as_ref());
            assert_eq!(artifact["toolName"], "read");
            assert_eq!(artifact["sourceKind"], "selectedTextWindow");
            let path_value = artifact_str_field(artifact, "path");
            assert!(!path_value.is_empty(), "artifact path must be a string");
            let path = PathBuf::from(path_value);
            let spilled = match std::fs::read_to_string(&path) {
                Ok(spilled) => spilled,
                Err(err) => {
                    assert!(false, "read spilled artifact {}: {err}", path.display());
                    return;
                }
            };
            let prefix = "    1→";
            assert_eq!(spilled.len(), prefix.len() + DEFAULT_MAX_BYTES);
            assert_eq!(
                artifact["byteCount"].as_u64().unwrap(),
                u64::try_from(spilled.len()).unwrap()
            );
            assert!(spilled.starts_with(prefix));
            assert!(spilled[prefix.len()..].bytes().all(|byte| byte == b'r'));
            assert_eq!(
                artifact["retentionClass"],
                TOOL_OUTPUT_ARTIFACT_RETENTION_CLASS
            );
            assert_eq!(
                artifact["spilloverReason"],
                TOOL_OUTPUT_ARTIFACT_SPILLOVER_REASON
            );
            assert_eq!(artifact["safeDeleteCandidate"], true);
        });
    }

    #[test]
    fn bash_tool_spills_truncated_full_output_to_artifact() {
        asupersync::test_utils::run_test(|| async {
            if !Path::new("/dev/zero").exists() {
                return;
            }

            let tmp = tempfile::tempdir().expect("workspace");
            let artifact_root = tempfile::tempdir().expect("artifact root");

            let bash_tool = BashTool::with_artifact_root(tmp.path(), artifact_root.path());
            let output = bash_tool
                .execute(
                    "bash-artifact-call",
                    serde_json::json!({
                        "command": "head -c 1001000 /dev/zero | tr '\\0' x",
                        "timeout": 10
                    }),
                    None,
                )
                .await
                .expect("bash large output");

            assert!(first_text(&output).contains("Full tool output artifact:"));
            let artifact = artifact_json(output.details.as_ref());
            assert_eq!(artifact["toolName"], "bash");
            assert_eq!(artifact["sourceKind"], "fullCommandOutput");
            let path = PathBuf::from(artifact_str_field(artifact, "path"));
            assert_eq!(std::fs::metadata(path).unwrap().len(), 1_001_000);
            assert_eq!(artifact["redactionSummary"]["status"], "clean");
            assert_eq!(artifact["safeDeleteCandidate"], true);
        });
    }

    #[test]
    fn bash_tool_redacts_secret_like_full_output_artifacts() {
        asupersync::test_utils::run_test(|| async {
            if !Path::new("/dev/zero").exists() {
                return;
            }

            let tmp = tempfile::tempdir().expect("workspace");
            let artifact_root = tempfile::tempdir().expect("artifact root");
            let leaked_token = "sk-bashredactionfixture1234567890";

            let bash_tool = BashTool::with_artifact_root(tmp.path(), artifact_root.path());
            let output = bash_tool
                .execute(
                    "bash-secret-artifact-call",
                    serde_json::json!({
                        "command": format!("printf 'API_TOKEN={leaked_token}\\n'; head -c 1001000 /dev/zero | tr '\\0' x"),
                        "timeout": 10
                    }),
                    None,
                )
                .await
                .expect("bash large output");

            assert!(first_text(&output).contains("Full tool output artifact:"));
            let artifact = artifact_json(output.details.as_ref());
            assert_eq!(artifact["toolName"], "bash");
            assert_eq!(artifact["redactionSummary"]["status"], "redacted");
            assert_eq!(artifact["redactionSummary"]["rawSecretBytesEmitted"], 0);
            let path = PathBuf::from(artifact_str_field(artifact, "path"));
            let persisted = std::fs::read_to_string(path).expect("read redacted bash artifact");
            assert!(!persisted.contains(leaked_token));
            assert!(persisted.contains("API_TOKEN=[REDACTED]"));
        });
    }

    #[test]
    fn grep_tool_spills_large_search_results_with_lifecycle_manifest() {
        asupersync::test_utils::run_test(|| async {
            if !rg_available() {
                return;
            }

            let tmp = tempfile::tempdir().expect("workspace");
            let artifact_root = tempfile::tempdir().expect("artifact root");
            let mut body = String::new();
            let suffix = "g".repeat(560);
            for idx in 0..2200 {
                let _ = writeln!(body, "target {idx:04} {suffix}");
            }
            std::fs::write(tmp.path().join("large-grep.txt"), body).expect("write grep fixture");

            let grep_tool = GrepTool::with_artifact_root(tmp.path(), artifact_root.path());
            let output = grep_tool
                .execute(
                    "grep-artifact-call",
                    serde_json::json!({
                        "pattern": "target",
                        "path": "large-grep.txt",
                        "literal": true,
                        "limit": 2200
                    }),
                    None,
                )
                .await
                .expect("grep large output");

            assert!(first_text(&output).contains("Full tool output artifact:"));
            let artifact = artifact_json(output.details.as_ref());
            assert_eq!(artifact["toolName"], "grep");
            assert_eq!(artifact["sourceKind"], "searchResults");
            assert_eq!(
                artifact["retentionClass"],
                TOOL_OUTPUT_ARTIFACT_RETENTION_CLASS
            );
            assert_eq!(artifact["safeDeleteCandidate"], true);
            assert_eq!(artifact["redactionSummary"]["status"], "clean");
            let path = PathBuf::from(artifact_str_field(artifact, "path"));
            let persisted = std::fs::read_to_string(path).expect("read grep artifact");
            assert!(persisted.contains("large-grep.txt:1: target 0000"));
            assert!(
                artifact["byteCount"].as_u64().unwrap()
                    > u64::try_from(TOOL_OUTPUT_ARTIFACT_THRESHOLD_BYTES).unwrap()
            );
        });
    }

    #[test]
    fn read_tool_denied_path_does_not_emit_lifecycle_artifact() {
        asupersync::test_utils::run_test(|| async {
            let cwd = tempfile::tempdir().expect("workspace");
            let outside = tempfile::tempdir().expect("outside");
            let artifact_root = tempfile::tempdir().expect("artifact root");
            let outside_path = outside.path().join("secret.txt");
            std::fs::write(&outside_path, "API_TOKEN=sk-deniedpathfixture1234567890")
                .expect("outside secret");

            let read_tool = ReadTool::with_artifact_root(cwd.path(), artifact_root.path());
            let err = read_tool
                .execute(
                    "read-denied-artifact-call",
                    serde_json::json!({ "path": outside_path }),
                    None,
                )
                .await
                .expect_err("outside read should be denied");

            assert!(
                err.to_string()
                    .contains("Cannot read outside the working directory or agent dir")
            );
            let mut entries = std::fs::read_dir(artifact_root.path()).expect("artifact root");
            assert!(
                entries.next().is_none(),
                "denied reads must not write artifacts"
            );
        });
    }

    #[test]
    fn ls_tool_spills_oversized_directory_listing_to_artifact() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().expect("workspace");
            let artifact_root = tempfile::tempdir().expect("artifact root");
            let suffix = "x".repeat(224);
            for i in 0..4_500 {
                let name = format!("entry-{i:04}-{suffix}.txt");
                std::fs::write(tmp.path().join(name), "").expect("write listing fixture");
            }

            let ls_tool = LsTool::with_artifact_root(tmp.path(), artifact_root.path());
            let output = ls_tool
                .execute(
                    "ls-artifact-call",
                    serde_json::json!({ "path": ".", "limit": 4500 }),
                    None,
                )
                .await
                .expect("ls large directory");

            assert!(first_text(&output).contains("Full tool output artifact:"));
            let artifact = artifact_json(output.details.as_ref());
            assert_eq!(artifact["toolName"], "ls");
            assert_eq!(artifact["sourceKind"], "directoryEntries");
            assert!(
                artifact["byteCount"].as_u64().unwrap()
                    > u64::try_from(TOOL_OUTPUT_ARTIFACT_THRESHOLD_BYTES).unwrap()
            );
            let path = PathBuf::from(artifact_str_field(artifact, "path"));
            assert!(
                std::fs::read_to_string(path)
                    .unwrap()
                    .contains("entry-0000-")
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn cache_permission_fingerprints_do_not_read_mode_denied_content() {
        let tmp = tempfile::tempdir().expect("cache permission workspace");
        let denied_file = tmp.path().join("owner-denied.txt");
        std::fs::write(&denied_file, "cache secret").expect("write denied cache fixture");
        let file_guard = UnixModeGuard::set(&denied_file, 0o004);
        assert!(
            fingerprint_file_content(&denied_file).is_none(),
            "file fingerprinting must not borrow other-read for an owner-denied file"
        );
        drop(file_guard);

        let denied_dir = tmp.path().join("denied-tree");
        std::fs::create_dir(&denied_dir).expect("create denied cache directory");
        std::fs::write(denied_dir.join("secret.txt"), "recursive cache secret")
            .expect("write recursive cache fixture");
        let dir_guard = UnixModeGuard::set(&denied_dir, 0o000);
        assert!(
            fingerprint_directory_recursive(tmp.path()).is_none(),
            "recursive fingerprinting must stop before a mode-denied subtree"
        );
        drop(dir_guard);
    }

    #[cfg(unix)]
    #[test]
    fn global_git_ignore_resolution_short_circuits_xdg_after_home_match() {
        let tmp = tempfile::tempdir().expect("global git config fixture");
        let home = tmp.path().join("home");
        let xdg = tmp.path().join("xdg");
        std::fs::create_dir_all(xdg.join("git")).expect("create xdg git config directory");
        std::fs::create_dir(&home).expect("create home directory");
        let home_config = home.join(".gitconfig");
        std::fs::write(
            &home_config,
            "[core]\n    excludesFile = ~/home-global-ignore\n",
        )
        .expect("write home git config");
        let xdg_config = xdg.join("git").join("config");
        std::fs::write(
            &xdg_config,
            "[core]\n    excludesFile = /must-not-be-consumed\n",
        )
        .expect("write xdg git config");
        let _xdg_mode_guard = UnixModeGuard::set(&xdg_config, 0o000);
        let locations = GitGlobalConfigLocations {
            home_dir: Some(home.clone()),
            xdg_config_home: Some(xdg),
        };
        let access_context = EffectiveModeAccessContext::current().expect("effective identity");

        let controls = resolve_git_global_ignore_controls(
            &locations,
            &access_context,
            "global git config test",
        )
        .expect("a home match must short-circuit the denied xdg config");
        assert_eq!(controls.consumed_config_paths, vec![home_config]);
        assert_eq!(controls.ignore_path, Some(home.join("home-global-ignore")));
    }

    #[test]
    fn global_git_ignore_parser_matches_broad_tilde_expansion_with_a_bound() {
        let home = Path::new("/synthetic/home");
        let candidate = "prefix~middle~suffix";
        let config = format!("[core]\n    excludesFile = {candidate}\n");
        let parsed = parse_gitconfig_excludes_path(config.as_bytes(), Some(home))
            .expect("bounded tilde expansion")
            .expect("configured excludesFile");
        assert_eq!(
            parsed,
            PathBuf::from(candidate.replace('~', &home.to_string_lossy()))
        );

        let expansion_unit = "h".repeat(128);
        let tilde_count = GIT_GLOBAL_IGNORE_PATH_MAX_BYTES / expansion_unit.len() + 1;
        let oversized = format!("[core]\n    excludesFile = {}\n", "~".repeat(tilde_count));
        let error =
            parse_gitconfig_excludes_path(oversized.as_bytes(), Some(Path::new(&expansion_unit)))
                .expect_err("broad replacement must not amplify beyond the path bound");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("expanded"));
    }

    #[cfg(not(windows))]
    #[test]
    fn fd_global_ignore_resolution_uses_xdg_strategy_on_unix_and_macos() {
        let locations = GitGlobalConfigLocations {
            home_dir: Some(PathBuf::from("/synthetic/home")),
            xdg_config_home: Some(PathBuf::from("/synthetic/xdg")),
        };
        assert_eq!(
            fd_global_ignore_path_from_locations(&locations),
            Some(PathBuf::from("/synthetic/xdg/fd/ignore"))
        );

        let fallback = GitGlobalConfigLocations {
            home_dir: Some(PathBuf::from("/synthetic/home")),
            xdg_config_home: Some(PathBuf::from("/synthetic/home/.config")),
        };
        assert_eq!(
            fd_global_ignore_path_from_locations(&fallback),
            Some(PathBuf::from("/synthetic/home/.config/fd/ignore"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn global_git_ignore_resolution_rejects_mode_denied_consumed_configs() {
        let tmp = tempfile::tempdir().expect("denied global git config fixture");
        let home = tmp.path().join("home");
        let xdg = tmp.path().join("xdg");
        std::fs::create_dir_all(xdg.join("git")).expect("create xdg git config directory");
        std::fs::create_dir(&home).expect("create home directory");
        let locations = GitGlobalConfigLocations {
            home_dir: Some(home.clone()),
            xdg_config_home: Some(xdg.clone()),
        };
        let access_context = EffectiveModeAccessContext::current().expect("effective identity");

        let home_config = home.join(".gitconfig");
        std::fs::write(&home_config, "[core]\n").expect("write home git config");
        let home_guard = UnixModeGuard::set(&home_config, 0o004);
        let error = resolve_git_global_ignore_controls(
            &locations,
            &access_context,
            "global git config test",
        )
        .expect_err("the selected owner class must reject a denied home config");
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        drop(home_guard);

        let xdg_config = xdg.join("git").join("config");
        std::fs::write(
            &xdg_config,
            "[core]\n    excludesFile = /xdg-global-ignore\n",
        )
        .expect("write xdg git config");
        let _xdg_guard = UnixModeGuard::set(&xdg_config, 0o004);
        let error = resolve_git_global_ignore_controls(
            &locations,
            &access_context,
            "global git config test",
        )
        .expect_err("xdg is consumed when home has no excludesFile and must be mode-checked");
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[cfg(unix)]
    #[test]
    fn global_git_ignore_resolution_rejects_oversized_consumed_config() {
        let tmp = tempfile::tempdir().expect("oversized global git config fixture");
        let home = tmp.path().join("home");
        std::fs::create_dir(&home).expect("create home directory");
        let config = home.join(".gitconfig");
        let oversized_len = usize::try_from(GIT_GLOBAL_CONFIG_MAX_BYTES)
            .expect("global config limit fits usize")
            .saturating_add(1);
        std::fs::write(&config, vec![b'x'; oversized_len])
            .expect("write oversized global git config");
        let access_context = EffectiveModeAccessContext::current().expect("effective identity");

        let error = resolve_git_global_ignore_controls(
            &GitGlobalConfigLocations {
                home_dir: Some(home),
                xdg_config_home: None,
            },
            &access_context,
            "global git config test",
        )
        .expect_err("an oversized consumed config must not be read without a bound");

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("exceeds"));
        assert!(error.to_string().contains(&config.display().to_string()));
    }

    #[test]
    fn recursive_scan_rejects_oversized_ignore_control_before_walking() {
        let tmp = tempfile::tempdir().expect("oversized ignore-control fixture");
        let control = tmp.path().join(".gitignore");
        let file = std::fs::File::create(&control).expect("create sparse ignore control");
        file.set_len(RECURSIVE_SCAN_IGNORE_CONTROL_MAX_BYTES.saturating_add(1))
            .expect("size sparse ignore control");
        let access_context = EffectiveModeAccessContext::current().expect("effective identity");

        let error =
            ensure_ignore_control_readable_sync(&control, &access_context, "recursive scan test")
                .expect_err("the in-process ignore parser must have an explicit input bound");

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("exceeds"));
        assert!(error.to_string().contains(&control.display().to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn recursive_scan_applies_relative_global_git_ignore_from_command_cwd() {
        let tmp = tempfile::tempdir().expect("relative global ignore fixture");
        let cwd = tmp.path().join("workspace");
        let home = tmp.path().join("home");
        let search_root = cwd.join("scan-root");
        let ignored = search_root.join("ignored-vault");
        std::fs::create_dir_all(&ignored).expect("create ignored directory");
        std::fs::create_dir(&home).expect("create home directory");
        std::fs::create_dir(cwd.join("config")).expect("create relative ignore directory");
        std::fs::write(search_root.join("visible.txt"), "needle\n").expect("write visible file");
        std::fs::write(ignored.join("secret.txt"), "needle\n").expect("write ignored file");
        std::fs::write(
            home.join(".gitconfig"),
            "[core]\n    excludesFile = config/global.ignore\n",
        )
        .expect("write relative global git config");
        std::fs::write(
            cwd.join("config").join("global.ignore"),
            "scan-root/ignored-vault/\n",
        )
        .expect("write relative global ignore file");

        let access_context = EffectiveModeAccessContext::current().expect("effective identity");
        let controls = resolve_git_global_ignore_controls(
            &GitGlobalConfigLocations {
                home_dir: Some(home),
                xdg_config_home: None,
            },
            &access_context,
            "relative global ignore test",
        )
        .expect("resolve relative global ignore");
        let global_ignore = ignore_control_path_from_command_cwd(
            controls.ignore_path.expect("configured global ignore"),
            &cwd,
        );
        ensure_ignore_control_readable_sync(
            &global_ignore,
            &access_context,
            "relative global ignore test",
        )
        .expect("validate relative global ignore");

        let walker = recursive_scan_walk_builder(
            &search_root,
            &cwd,
            RecursiveScanAccess::ReadableFiles,
            None,
            Some(&global_ignore),
        )
        .expect("build recursive walker")
        .build();
        let visited = walker
            .map(|entry| entry.expect("walk entry").into_path())
            .collect::<Vec<_>>();

        assert!(visited.contains(&search_root.join("visible.txt")));
        assert!(!visited.contains(&ignored));
        assert!(!visited.contains(&ignored.join("secret.txt")));
    }

    #[cfg(unix)]
    #[test]
    fn recursive_scan_accepts_linked_worktree_git_file() {
        let tmp = tempfile::tempdir().expect("linked worktree fixture");
        let worktree = tmp.path().join("worktree");
        std::fs::create_dir(&worktree).expect("create linked worktree");
        std::fs::write(
            worktree.join(".git"),
            "gitdir: /tmp/example-git-common-dir/worktrees/example\n",
        )
        .expect("write linked-worktree .git file");
        std::fs::write(worktree.join("visible.txt"), "needle\n").expect("write visible file");

        for access in [
            RecursiveScanAccess::ReadableFiles,
            RecursiveScanAccess::DirectoriesOnly,
        ] {
            ensure_recursive_scan_access_sync(
                &worktree,
                tmp.path(),
                "linked worktree scan",
                access,
                None,
            )
            .expect("a regular .git file must not make .git/info/exclude an ENOTDIR failure");
        }
    }

    #[cfg(unix)]
    #[test]
    fn recursive_grep_glob_is_rooted_at_command_cwd() {
        let tmp = tempfile::tempdir().expect("grep glob fixture");
        let search_root = tmp.path().join("scan-root");
        let nested = search_root.join("a");
        std::fs::create_dir_all(&nested).expect("create nested scan root");
        let denied = nested.join("owner-denied.txt");
        std::fs::write(&denied, "needle\n").expect("write denied glob fixture");
        let _mode_guard = UnixModeGuard::set(&denied, 0o004);

        let error = ensure_recursive_scan_access_sync(
            &search_root,
            tmp.path(),
            "recursive content scanning",
            RecursiveScanAccess::ReadableFiles,
            Some("scan-root/a/*.txt"),
        )
        .expect_err("cwd-relative rg glob includes the owner-denied file");
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);

        ensure_recursive_scan_access_sync(
            &search_root,
            tmp.path(),
            "recursive content scanning",
            RecursiveScanAccess::ReadableFiles,
            Some("a/*.txt"),
        )
        .expect("a slash glob that rg evaluates outside its cwd-relative surface must not deny");
    }

    #[cfg(unix)]
    #[test]
    fn recursive_scan_applies_explicit_root_ignore_from_command_cwd() {
        let tmp = tempfile::tempdir().expect("explicit ignore fixture");
        let search_root = tmp.path().join("scan-root");
        let ignored = search_root.join("ignored-vault");
        std::fs::create_dir_all(&ignored).expect("create ignored directory");
        std::fs::write(ignored.join("secret.txt"), "needle\n").expect("write ignored file");
        std::fs::write(search_root.join(".gitignore"), "scan-root/ignored-vault/\n")
            .expect("write cwd-relative explicit ignore rule");
        let _mode_guard = UnixModeGuard::set(&ignored, 0o000);

        ensure_recursive_scan_access_sync(
            &search_root,
            tmp.path(),
            "recursive content scanning",
            RecursiveScanAccess::ReadableFiles,
            None,
        )
        .expect("the preflight must honor the same explicit --ignore-file surface as rg");
    }

    #[cfg(unix)]
    #[test]
    fn recursive_grep_validates_ignore_control_hidden_by_positive_glob() {
        let tmp = tempfile::tempdir().expect("nested ignore control fixture");
        let search_root = tmp.path().join("scan-root");
        let nested = search_root.join("nested");
        std::fs::create_dir_all(&nested).expect("create nested directory");
        std::fs::write(nested.join("visible.txt"), "needle\n").expect("write visible file");
        let ignore_control = nested.join(".gitignore");
        std::fs::write(&ignore_control, "ignored.txt\n").expect("write nested ignore control");
        let _mode_guard = UnixModeGuard::set(&ignore_control, 0o004);

        let error = ensure_recursive_scan_access_sync(
            &search_root,
            tmp.path(),
            "recursive content scanning",
            RecursiveScanAccess::ReadableFiles,
            Some("scan-root/**/*.txt"),
        )
        .expect_err("rg consumes a nested ignore control even when its positive glob hides it");
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[cfg(unix)]
    #[test]
    fn permission_scan_path_checks_lexical_parent_erased_by_terminal_symlink() {
        use std::os::unix::fs::symlink;

        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().expect("terminal symlink fixture");
            let target = tmp.path().join("target");
            std::fs::create_dir(&target).expect("create accessible target");
            let blocked_parent = tmp.path().join("blocked-parent");
            std::fs::create_dir(&blocked_parent).expect("create blocked lexical parent");
            let lexical_path = blocked_parent.join("scan-link");
            symlink(&target, &lexical_path).expect("create terminal symlink");
            let canonical_path = std::fs::canonicalize(&lexical_path).expect("canonical target");
            let _mode_guard = UnixModeGuard::set(&blocked_parent, 0o000);

            let error = ensure_scan_path_ancestors_searchable(&lexical_path, &canonical_path)
                .await
                .expect_err("a mode-denied lexical parent must survive canonical scope checks");
            assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
            assert!(
                error
                    .to_string()
                    .contains(&blocked_parent.display().to_string()),
                "permission error must identify the denied lexical ancestor: {error}"
            );
        });
    }

    async fn assert_read_cache_hit_and_stale(tmp: &Path) {
        let note = tmp.join("note.txt");
        std::fs::write(&note, "alpha\n").expect("write note");

        let read_tool = ReadTool::new(tmp);
        let read_input = serde_json::json!({ "path": "note.txt" });
        let first = read_tool
            .execute("read-1", read_input.clone(), None)
            .await
            .expect("first read");
        assert!(first_text(&first).contains("alpha"));

        assert_eventual_cache_hit(
            &read_tool,
            "read-2",
            &read_input,
            first_text(&first),
            "read",
        )
        .await;

        let invalidations_before = tool_output_cache_stats_for_tests().invalidations;
        std::fs::write(&note, "beta\n").expect("rewrite note");
        let third = read_tool
            .execute("read-3", read_input.clone(), None)
            .await
            .expect("invalidated read");
        assert!(first_text(&third).contains("beta"));
        assert!(!first_text(&third).contains("alpha"));
        assert!(tool_output_cache_stats_for_tests().invalidations > invalidations_before);
    }

    #[cfg(unix)]
    async fn assert_read_cache_does_not_bind_opened_inode_to_replacement_path(tmp: &Path) {
        let target = tmp.join("read-cache-race.txt");
        let retained_original = tmp.join("read-cache-race-opened-original.txt");
        std::fs::write(&target, "opened inode payload\n").expect("write original read target");

        let target_for_hook = target.clone();
        let retained_for_hook = retained_original.clone();
        let raced_tool = ReadTool::with_after_open_hook(tmp, move || {
            std::fs::rename(&target_for_hook, &retained_for_hook)
                .expect("retain the already-open read target");
            std::fs::write(&target_for_hook, "replacement path payload\n")
                .expect("install replacement read target");
        });
        let input = serde_json::json!({ "path": "read-cache-race.txt" });
        let opened_output = raced_tool
            .execute("read-cache-race-opened", input.clone(), None)
            .await
            .expect("read the already-open inode");
        assert!(first_text(&opened_output).contains("opened inode payload"));
        assert!(!first_text(&opened_output).contains("replacement path payload"));

        let replacement_output = ReadTool::new(tmp)
            .execute("read-cache-race-replacement", input, None)
            .await
            .expect("read the replacement pathname");
        assert!(first_text(&replacement_output).contains("replacement path payload"));
        assert!(!first_text(&replacement_output).contains("opened inode payload"));
        assert_eq!(
            std::fs::read_to_string(&retained_original).expect("read retained original"),
            "opened inode payload\n"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[allow(clippy::too_many_lines)]
    async fn assert_scoped_scan_roots_survive_after_open_replacement(tmp: &Path) {
        use std::os::unix::fs::symlink;

        let outside = tempfile::tempdir().expect("create outside scan fixture");

        if find_rg_binary().is_some() {
            let cwd_root = tmp.join("grep-pinned-cwd");
            let retained_cwd = tmp.join("grep-pinned-cwd-retained");
            std::fs::create_dir_all(cwd_root.join("src")).expect("create pinned grep cwd");
            std::fs::write(
                cwd_root.join("src").join("context.txt"),
                "before context\ninside cwd sentinel\nafter context\n",
            )
            .expect("write pinned cwd grep fixture");
            let outside_cwd = outside.path().join("grep-cwd-outside");
            std::fs::create_dir_all(outside_cwd.join("src")).expect("create replacement grep cwd");
            std::fs::write(outside_cwd.join(".gitignore"), "src/\n")
                .expect("write replacement cwd ignore control");
            std::fs::write(
                outside_cwd.join("src").join("outside.txt"),
                "outside cwd sentinel\n",
            )
            .expect("write replacement cwd grep fixture");

            let root_for_hook = cwd_root.clone();
            let retained_for_hook = retained_cwd.clone();
            let outside_for_hook = outside_cwd.clone();
            let grep = GrepTool::with_after_scope_hook(&cwd_root, move || {
                std::fs::rename(&root_for_hook, &retained_for_hook)
                    .expect("retain pinned grep cwd");
                symlink(&outside_for_hook, &root_for_hook)
                    .expect("replace grep cwd with outside symlink");
            });
            let output = grep
                .execute(
                    "grep-pinned-cwd-race",
                    serde_json::json!({
                        "pattern": "cwd sentinel",
                        "glob": "src/**/*.txt",
                        "context": 1
                    }),
                    None,
                )
                .await
                .expect("grep pinned default cwd");
            let text = first_text(&output);
            assert!(text.contains("before context"), "{text}");
            assert!(text.contains("inside cwd sentinel"), "{text}");
            assert!(text.contains("after context"), "{text}");
            assert!(!text.contains("outside cwd sentinel"), "{text}");
            assert!(!text.contains("unable to read file"), "{text}");
            assert!(!text.contains("/proc/self/fd"), "{text}");
            assert!(!text.contains("/dev/fd"), "{text}");
        }

        if find_fd_binary().is_some() {
            let cwd_root = tmp.join("find-pinned-cwd");
            let retained_cwd = tmp.join("find-pinned-cwd-retained");
            std::fs::create_dir_all(cwd_root.join("src")).expect("create pinned find cwd");
            std::fs::write(cwd_root.join("src").join("inside.txt"), "inside\n")
                .expect("write pinned cwd find fixture");
            let outside_cwd = outside.path().join("find-cwd-outside");
            std::fs::create_dir_all(outside_cwd.join("src")).expect("create replacement find cwd");
            std::fs::write(outside_cwd.join(".gitignore"), "src/\n")
                .expect("write replacement find ignore control");
            std::fs::write(outside_cwd.join("src").join("outside.txt"), "outside\n")
                .expect("write replacement cwd find fixture");

            let root_for_hook = cwd_root.clone();
            let retained_for_hook = retained_cwd.clone();
            let outside_for_hook = outside_cwd.clone();
            let find = FindTool::with_after_scope_hook(&cwd_root, move || {
                std::fs::rename(&root_for_hook, &retained_for_hook)
                    .expect("retain pinned find cwd");
                symlink(&outside_for_hook, &root_for_hook)
                    .expect("replace find cwd with outside symlink");
            });
            let output = find
                .execute(
                    "find-pinned-cwd-race",
                    serde_json::json!({ "pattern": "src/**/*.txt" }),
                    None,
                )
                .await
                .expect("find pinned default cwd");
            let text = first_text(&output);
            assert!(text.contains("src/inside.txt"), "{text}");
            assert!(!text.contains("outside.txt"), "{text}");
            assert!(!text.contains("/proc/self/fd"), "{text}");
            assert!(!text.contains("/dev/fd"), "{text}");
        }

        {
            let cwd_root = tmp.join("ls-pinned-cwd");
            let retained_cwd = tmp.join("ls-pinned-cwd-retained");
            std::fs::create_dir(&cwd_root).expect("create pinned ls cwd");
            std::fs::write(cwd_root.join("inside.txt"), "inside\n")
                .expect("write pinned cwd ls fixture");
            let outside_cwd = outside.path().join("ls-cwd-outside");
            std::fs::create_dir(&outside_cwd).expect("create replacement ls cwd");
            std::fs::write(outside_cwd.join("outside.txt"), "outside\n")
                .expect("write replacement cwd ls fixture");

            let root_for_hook = cwd_root.clone();
            let retained_for_hook = retained_cwd.clone();
            let outside_for_hook = outside_cwd.clone();
            let ls = LsTool::with_after_scope_hook(&cwd_root, move || {
                std::fs::rename(&root_for_hook, &retained_for_hook).expect("retain pinned ls cwd");
                symlink(&outside_for_hook, &root_for_hook)
                    .expect("replace ls cwd with outside symlink");
            });
            let output = ls
                .execute("ls-pinned-cwd-race", serde_json::json!({}), None)
                .await
                .expect("list pinned default cwd");
            let text = first_text(&output);
            assert!(text.contains("inside.txt"), "{text}");
            assert!(!text.contains("outside.txt"), "{text}");
        }

        if find_rg_binary().is_some() {
            let grep_root = tmp.join("grep-pinned-root");
            let retained_grep_root = tmp.join("grep-pinned-root-retained");
            std::fs::create_dir(&grep_root).expect("create grep root");
            std::fs::write(grep_root.join("inside.txt"), "inside grep sentinel\n")
                .expect("write inside grep fixture");
            let outside_grep = outside.path().join("grep-outside");
            std::fs::create_dir(&outside_grep).expect("create outside grep root");
            std::fs::write(outside_grep.join("outside.txt"), "outside grep sentinel\n")
                .expect("write outside grep fixture");

            let root_for_hook = grep_root.clone();
            let retained_for_hook = retained_grep_root.clone();
            let outside_for_hook = outside_grep.clone();
            let grep = GrepTool::with_after_scope_hook(tmp, move || {
                std::fs::rename(&root_for_hook, &retained_for_hook)
                    .expect("retain pinned grep root");
                symlink(&outside_for_hook, &root_for_hook)
                    .expect("replace grep path with outside symlink");
            });
            let output = grep
                .execute(
                    "grep-pinned-root-race",
                    serde_json::json!({
                        "pattern": "sentinel",
                        "path": "grep-pinned-root"
                    }),
                    None,
                )
                .await
                .expect("grep pinned directory root");
            let text = first_text(&output);
            assert!(text.contains("inside grep sentinel"), "{text}");
            assert!(!text.contains("outside grep sentinel"), "{text}");
            assert!(!text.contains("/proc/self/fd"), "{text}");
            assert!(!text.contains("/dev/fd"), "{text}");

            let grep_file = tmp.join("grep-pinned-file.txt");
            let retained_grep_file = tmp.join("grep-pinned-file-retained.txt");
            let outside_grep_file = outside.path().join("grep-outside-file.txt");
            std::fs::write(&grep_file, "inside file sentinel\n").expect("write inside grep file");
            std::fs::write(&outside_grep_file, "outside file sentinel\n")
                .expect("write outside grep file");
            let file_for_hook = grep_file.clone();
            let retained_file_for_hook = retained_grep_file.clone();
            let outside_file_for_hook = outside_grep_file.clone();
            let grep = GrepTool::with_after_scope_hook(tmp, move || {
                std::fs::rename(&file_for_hook, &retained_file_for_hook)
                    .expect("retain pinned grep file");
                symlink(&outside_file_for_hook, &file_for_hook)
                    .expect("replace grep file with outside symlink");
            });
            let output = grep
                .execute(
                    "grep-pinned-file-race",
                    serde_json::json!({
                        "pattern": "sentinel",
                        "path": "grep-pinned-file.txt"
                    }),
                    None,
                )
                .await
                .expect("grep pinned file root");
            let text = first_text(&output);
            assert!(text.contains("inside file sentinel"), "{text}");
            assert!(!text.contains("outside file sentinel"), "{text}");
            assert!(!text.contains("/proc/self/fd"), "{text}");
            assert!(!text.contains("/dev/fd"), "{text}");
        }

        if find_fd_binary().is_some() {
            let find_root = tmp.join("find-pinned-root");
            let retained_find_root = tmp.join("find-pinned-root-retained");
            std::fs::create_dir(&find_root).expect("create find root");
            std::fs::write(find_root.join("inside-find.txt"), "inside\n")
                .expect("write inside find fixture");
            let outside_find = outside.path().join("find-outside");
            std::fs::create_dir(&outside_find).expect("create outside find root");
            std::fs::write(outside_find.join("outside-find.txt"), "outside\n")
                .expect("write outside find fixture");

            let root_for_hook = find_root.clone();
            let retained_for_hook = retained_find_root.clone();
            let outside_for_hook = outside_find.clone();
            let find = FindTool::with_after_scope_hook(tmp, move || {
                std::fs::rename(&root_for_hook, &retained_for_hook)
                    .expect("retain pinned find root");
                symlink(&outside_for_hook, &root_for_hook)
                    .expect("replace find path with outside symlink");
            });
            let output = find
                .execute(
                    "find-pinned-root-race",
                    serde_json::json!({
                        "pattern": "*.txt",
                        "path": "find-pinned-root"
                    }),
                    None,
                )
                .await
                .expect("find pinned directory root");
            let text = first_text(&output);
            assert!(text.contains("inside-find.txt"), "{text}");
            assert!(!text.contains("outside-find.txt"), "{text}");
            assert!(!text.contains("/proc/self/fd"), "{text}");
            assert!(!text.contains("/dev/fd"), "{text}");
        }

        let ls_root = tmp.join("ls-pinned-root");
        let retained_ls_root = tmp.join("ls-pinned-root-retained");
        std::fs::create_dir(&ls_root).expect("create ls root");
        std::fs::write(ls_root.join("inside-ls.txt"), "inside\n").expect("write inside ls fixture");
        let outside_ls = outside.path().join("ls-outside");
        std::fs::create_dir(&outside_ls).expect("create outside ls root");
        std::fs::write(outside_ls.join("outside-ls.txt"), "outside\n")
            .expect("write outside ls fixture");

        let root_for_hook = ls_root.clone();
        let retained_for_hook = retained_ls_root.clone();
        let outside_for_hook = outside_ls.clone();
        let ls = LsTool::with_after_scope_hook(tmp, move || {
            std::fs::rename(&root_for_hook, &retained_for_hook).expect("retain pinned ls root");
            symlink(&outside_for_hook, &root_for_hook)
                .expect("replace ls path with outside symlink");
        });
        let output = ls
            .execute(
                "ls-pinned-root-race",
                serde_json::json!({ "path": "ls-pinned-root" }),
                None,
            )
            .await
            .expect("list pinned directory root");
        let text = first_text(&output);
        assert!(text.contains("inside-ls.txt"), "{text}");
        assert!(!text.contains("outside-ls.txt"), "{text}");
    }

    async fn assert_find_selects_globally_newest_match(tmp: &Path) {
        if find_fd_binary().is_none() {
            return;
        }
        let root = tmp.join("find-newest-complete-root");
        std::fs::create_dir(&root).expect("create complete find fixture");
        let old_time = filetime::FileTime::from_unix_time(1_600_000_000, 0);
        for index in 0..64 {
            let old = root.join(format!("a-old-{index:03}.txt"));
            std::fs::write(&old, "old\n").expect("write old find candidate");
            filetime::set_file_mtime(&old, old_time).expect("set old candidate mtime");
        }
        let newest = root.join("z-newest.txt");
        std::fs::write(&newest, "newest\n").expect("write newest find candidate");
        filetime::set_file_mtime(
            &newest,
            filetime::FileTime::from_unix_time(1_700_000_000, 0),
        )
        .expect("set newest candidate mtime");

        let output = FindTool::new(tmp)
            .execute(
                "find-global-newest",
                serde_json::json!({
                    "pattern": "*.txt",
                    "path": "find-newest-complete-root",
                    "limit": 1
                }),
                None,
            )
            .await
            .expect("find globally newest result");
        let text = first_text(&output);
        assert!(text.contains("z-newest.txt"), "{text}");
        assert!(!text.contains("a-old-"), "{text}");
    }

    /// Execute `tool` repeatedly until the process-global output cache reports
    /// a hit. The cache is shared across the whole test binary with a bounded
    /// LRU, so concurrently running tests can evict the freshly inserted entry
    /// between two calls; a miss re-inserts the entry, so a bounded retry
    /// converges unless caching is actually broken.
    async fn assert_eventual_cache_hit(
        tool: &dyn Tool,
        tool_call_id: &str,
        input: &serde_json::Value,
        expected_text: &str,
        label: &str,
    ) {
        for _attempt in 0..8 {
            let hits_before = tool_output_cache_stats_for_tests().hits;
            let output = tool
                .execute(tool_call_id, input.clone(), None)
                .await
                .unwrap_or_else(|err| panic!("{label}: cached execute failed: {err}"));
            assert_eq!(expected_text, first_text(&output), "{label}");
            if tool_output_cache_stats_for_tests().hits > hits_before {
                return;
            }
        }
        panic!("{label}: output was never served from the cache");
    }

    async fn assert_ls_cache_hit_and_stale(tmp: &Path) {
        let ls_tool = LsTool::new(tmp);
        let ls_input = serde_json::json!({ "path": "." });
        let ls_first = ls_tool
            .execute("ls-1", ls_input.clone(), None)
            .await
            .expect("first ls");
        assert!(first_text(&ls_first).contains("note.txt"));

        assert_eventual_cache_hit(&ls_tool, "ls-2", &ls_input, first_text(&ls_first), "ls").await;

        let invalidations_before = tool_output_cache_stats_for_tests().invalidations;
        std::fs::write(tmp.join("new.txt"), "new\n").expect("write new file");
        let ls_third = ls_tool
            .execute("ls-3", ls_input.clone(), None)
            .await
            .expect("invalidated ls");
        assert!(first_text(&ls_third).contains("new.txt"));
        assert!(tool_output_cache_stats_for_tests().invalidations > invalidations_before);
    }

    async fn assert_grep_cache_hit_and_stale_when_available(tmp: &Path) {
        if find_rg_binary().is_none() {
            return;
        }

        let grep_tool = GrepTool::new(tmp);
        let grep_input = serde_json::json!({ "pattern": "needle", "path": "." });
        std::fs::write(tmp.join("a.txt"), "needle\n").expect("write grep file");

        let grep_first = grep_tool
            .execute("grep-1", grep_input.clone(), None)
            .await
            .expect("first grep");
        assert!(first_text(&grep_first).contains("a.txt"));

        assert_eventual_cache_hit(
            &grep_tool,
            "grep-2",
            &grep_input,
            first_text(&grep_first),
            "grep",
        )
        .await;

        let invalidations_before = tool_output_cache_stats_for_tests().invalidations;
        std::fs::write(tmp.join("b.txt"), "needle\n").expect("write new match");
        let grep_third = grep_tool
            .execute("grep-3", grep_input.clone(), None)
            .await
            .expect("invalidated grep");
        assert!(first_text(&grep_third).contains("b.txt"));
        assert!(tool_output_cache_stats_for_tests().invalidations > invalidations_before);
    }

    async fn assert_find_cache_hit_and_stale_when_available(tmp: &Path) {
        if find_fd_binary().is_none() {
            return;
        }

        let find_tool = FindTool::new(tmp);
        let find_input = serde_json::json!({ "pattern": "*find*.txt", "path": "." });
        std::fs::write(tmp.join("find-a.txt"), "find\n").expect("write first find file");

        let find_first = find_tool
            .execute("find-1", find_input.clone(), None)
            .await
            .expect("first find");
        assert!(first_text(&find_first).contains("find-a.txt"));

        assert_eventual_cache_hit(
            &find_tool,
            "find-2",
            &find_input,
            first_text(&find_first),
            "find",
        )
        .await;

        let invalidations_before = tool_output_cache_stats_for_tests().invalidations;
        std::fs::write(tmp.join("find-b.txt"), "find\n").expect("write second find file");
        let find_third = find_tool
            .execute("find-3", find_input.clone(), None)
            .await
            .expect("invalidated find");
        assert!(first_text(&find_third).contains("find-b.txt"));
        assert!(tool_output_cache_stats_for_tests().invalidations > invalidations_before);
    }

    async fn assert_scan_caches_track_parent_ignore_control(tmp: &Path) {
        let have_rg = find_rg_binary().is_some();
        let have_fd = find_fd_binary().is_some();
        if !have_rg && !have_fd {
            return;
        }

        let search_root = tmp.join("parent-ignore-cache-root");
        std::fs::create_dir(&search_root).expect("create parent-ignore search root");
        std::fs::write(search_root.join("parent-grep.txt"), "parent cache needle\n")
            .expect("write parent-ignore grep fixture");
        std::fs::write(search_root.join("parent-find.txt"), "find fixture\n")
            .expect("write parent-ignore find fixture");
        let parent_ignore = tmp.join(".gitignore");
        std::fs::write(
            &parent_ignore,
            "parent-ignore-cache-root/parent-grep.txt\n\
             parent-ignore-cache-root/parent-find.txt\n",
        )
        .expect("write parent ignore control");

        let grep_tool = GrepTool::new(tmp);
        let grep_input = serde_json::json!({
            "pattern": "parent cache needle",
            "path": "parent-ignore-cache-root"
        });
        if have_rg {
            let first = grep_tool
                .execute("grep-parent-ignore-1", grep_input.clone(), None)
                .await
                .expect("first parent-ignore grep");
            assert!(!first_text(&first).contains("parent-grep.txt"));
            assert_eventual_cache_hit(
                &grep_tool,
                "grep-parent-ignore-2",
                &grep_input,
                first_text(&first),
                "parent-ignore grep",
            )
            .await;
        }

        let find_tool = FindTool::new(tmp);
        let find_input = serde_json::json!({
            "pattern": "parent-find.txt",
            "path": "parent-ignore-cache-root"
        });
        if have_fd {
            let first = find_tool
                .execute("find-parent-ignore-1", find_input.clone(), None)
                .await
                .expect("first parent-ignore find");
            assert!(!first_text(&first).contains("parent-find.txt"));
            assert_eventual_cache_hit(
                &find_tool,
                "find-parent-ignore-2",
                &find_input,
                first_text(&first),
                "parent-ignore find",
            )
            .await;
        }

        std::fs::write(&parent_ignore, "").expect("clear parent ignore control");
        if have_rg {
            let invalidations_before = tool_output_cache_stats_for_tests().invalidations;
            let third = grep_tool
                .execute("grep-parent-ignore-3", grep_input, None)
                .await
                .expect("invalidated parent-ignore grep");
            assert!(first_text(&third).contains("parent-grep.txt"));
            assert!(tool_output_cache_stats_for_tests().invalidations > invalidations_before);
        }
        if have_fd {
            let invalidations_before = tool_output_cache_stats_for_tests().invalidations;
            let third = find_tool
                .execute("find-parent-ignore-3", find_input, None)
                .await
                .expect("invalidated parent-ignore find");
            assert!(first_text(&third).contains("parent-find.txt"));
            assert!(tool_output_cache_stats_for_tests().invalidations > invalidations_before);
        }
    }

    #[cfg(unix)]
    async fn assert_scan_caches_track_symlinked_ignore_target(tmp: &Path) {
        use std::os::unix::fs::symlink;

        let have_rg = find_rg_binary().is_some();
        let have_fd = find_fd_binary().is_some();
        if !have_rg && !have_fd {
            return;
        }

        let workspace = tmp.join("symlink-ignore-workspace");
        let search_root = workspace.join("scan-root");
        std::fs::create_dir_all(&search_root).expect("create symlink-ignore search root");
        std::fs::write(
            search_root.join("symlink-grep.txt"),
            "symlink cache needle\n",
        )
        .expect("write symlink-ignore grep fixture");
        std::fs::write(search_root.join("symlink-find.txt"), "find fixture\n")
            .expect("write symlink-ignore find fixture");
        let ignore_target = tmp.join("symlink-ignore-target");
        std::fs::write(
            &ignore_target,
            "scan-root/symlink-grep.txt\nscan-root/symlink-find.txt\n",
        )
        .expect("write symlinked ignore target");
        symlink(&ignore_target, workspace.join(".gitignore"))
            .expect("create symlinked ignore control");

        let grep_tool = GrepTool::new(&workspace);
        let grep_input = serde_json::json!({
            "pattern": "symlink cache needle",
            "path": "scan-root"
        });
        if have_rg {
            let first = grep_tool
                .execute("grep-symlink-ignore-1", grep_input.clone(), None)
                .await
                .expect("first symlink-ignore grep");
            assert!(!first_text(&first).contains("symlink-grep.txt"));
            assert_eventual_cache_hit(
                &grep_tool,
                "grep-symlink-ignore-2",
                &grep_input,
                first_text(&first),
                "symlink-ignore grep",
            )
            .await;
        }

        let find_tool = FindTool::new(&workspace);
        let find_input = serde_json::json!({
            "pattern": "symlink-find.txt",
            "path": "scan-root"
        });
        if have_fd {
            let first = find_tool
                .execute("find-symlink-ignore-1", find_input.clone(), None)
                .await
                .expect("first symlink-ignore find");
            assert!(!first_text(&first).contains("symlink-find.txt"));
            assert_eventual_cache_hit(
                &find_tool,
                "find-symlink-ignore-2",
                &find_input,
                first_text(&first),
                "symlink-ignore find",
            )
            .await;
        }

        std::fs::write(&ignore_target, "").expect("clear symlinked ignore target");
        if have_rg {
            let invalidations_before = tool_output_cache_stats_for_tests().invalidations;
            let third = grep_tool
                .execute("grep-symlink-ignore-3", grep_input, None)
                .await
                .expect("invalidated symlink-ignore grep");
            assert!(first_text(&third).contains("symlink-grep.txt"));
            assert!(tool_output_cache_stats_for_tests().invalidations > invalidations_before);
        }
        if have_fd {
            let invalidations_before = tool_output_cache_stats_for_tests().invalidations;
            let third = find_tool
                .execute("find-symlink-ignore-3", find_input, None)
                .await
                .expect("invalidated symlink-ignore find");
            assert!(first_text(&third).contains("symlink-find.txt"));
            assert!(tool_output_cache_stats_for_tests().invalidations > invalidations_before);
        }
    }

    async fn assert_side_effect_tools_remain_uncached(tmp: &Path) {
        let side_effect_stats_before = tool_output_cache_stats_for_tests();
        let write_tool = WriteTool::new(tmp);
        write_tool
            .execute(
                "write-1",
                serde_json::json!({
                    "path": "side-effect.txt",
                    "content": "one\n"
                }),
                None,
            )
            .await
            .expect("write side-effect file");

        let edit_tool = EditTool::new(tmp);
        edit_tool
            .execute(
                "edit-1",
                serde_json::json!({
                    "path": "side-effect.txt",
                    "oldText": "one",
                    "newText": "two"
                }),
                None,
            )
            .await
            .expect("edit side-effect file");

        let bash_tool = BashTool::new(tmp);
        bash_tool
            .execute(
                "bash-1",
                serde_json::json!({
                    "command": "printf 'cache-uncached\\n'",
                    "timeout": 5
                }),
                None,
            )
            .await
            .expect("run uncached bash");

        let side_effect_stats_after = tool_output_cache_stats_for_tests();
        assert_eq!(
            (
                side_effect_stats_after.side_effect_accesses,
                side_effect_stats_after.side_effect_insert_attempts
            ),
            (
                side_effect_stats_before.side_effect_accesses,
                side_effect_stats_before.side_effect_insert_attempts
            ),
            "write, edit, and bash must not consult or populate the read-only output cache"
        );
    }

    #[test]
    fn tool_output_cache_reuses_and_invalidates_read_only_tool_outputs() {
        asupersync::test_utils::run_test(|| async {
            reset_tool_output_cache_for_tests();

            let tmp = tempfile::tempdir().expect("create temp dir");
            assert_read_cache_hit_and_stale(tmp.path()).await;
            #[cfg(unix)]
            assert_read_cache_does_not_bind_opened_inode_to_replacement_path(tmp.path()).await;
            // Root-swap survival needs fd-path traversal (procfs); macOS/BSD
            // /dev/fd nodes are dup-only, so those platforms scan by logical
            // path (Windows posture) and cannot hold this guarantee.
            #[cfg(any(target_os = "linux", target_os = "android"))]
            assert_scoped_scan_roots_survive_after_open_replacement(tmp.path()).await;
            assert_find_selects_globally_newest_match(tmp.path()).await;
            assert_ls_cache_hit_and_stale(tmp.path()).await;
            assert_grep_cache_hit_and_stale_when_available(tmp.path()).await;
            assert_find_cache_hit_and_stale_when_available(tmp.path()).await;
            assert_scan_caches_track_parent_ignore_control(tmp.path()).await;
            #[cfg(unix)]
            assert_scan_caches_track_symlinked_ignore_target(tmp.path()).await;
            assert_side_effect_tools_remain_uncached(tmp.path()).await;
        });
    }

    #[test]
    fn tool_output_context_cache_evidence_jsonl_covers_required_decisions()
    -> std::result::Result<(), String> {
        let evidence = include_str!("../docs/evidence/tool-output-context-cache.jsonl");
        let mut saw_read_hit = false;
        let mut saw_grep_stale = false;
        let mut saw_find_stale = false;
        let mut saw_ls_stale = false;
        let mut saw_write_uncached = false;
        let mut saw_edit_uncached = false;
        let mut saw_bash_uncached = false;

        for (line_number, line) in evidence.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }

            let event: serde_json::Value = serde_json::from_str(line).map_err(|err| {
                format!(
                    "invalid context-cache JSONL at line {}: {err}",
                    line_number + 1
                )
            })?;
            assert_eq!(
                event.get("schema").and_then(serde_json::Value::as_str),
                Some("pi.tool_output_context_cache.evidence.v1")
            );
            assert_eq!(
                event.get("bead").and_then(serde_json::Value::as_str),
                Some("bd-dklqn.1")
            );
            let related_beads = event
                .get("related_beads")
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| format!("missing related_beads at line {}", line_number + 1))?;
            assert!(
                related_beads
                    .iter()
                    .any(|bead| bead.as_str() == Some("bd-dklqn.2")),
                "evidence line {} must cover bd-dklqn.2",
                line_number + 1
            );

            let tool = event
                .get("tool")
                .and_then(serde_json::Value::as_str)
                .expect("tool");
            let outcome = event
                .get("outcome")
                .and_then(serde_json::Value::as_str)
                .expect("outcome");
            let reason = event
                .get("reason")
                .and_then(serde_json::Value::as_str)
                .expect("reason");

            match (tool, outcome, reason) {
                ("read", "hit", "unchanged_file_fingerprint") => saw_read_hit = true,
                ("grep", "stale", "recursive_directory_fingerprint_changed") => {
                    saw_grep_stale = true;
                }
                ("find", "stale", "recursive_directory_fingerprint_changed") => {
                    saw_find_stale = true;
                }
                ("ls", "stale", "directory_entry_fingerprint_changed") => saw_ls_stale = true,
                ("write", "uncached", "write_effect_tool") => saw_write_uncached = true,
                ("edit", "uncached", "write_effect_tool") => saw_edit_uncached = true,
                ("bash", "uncached", "process_effect_tool") => saw_bash_uncached = true,
                _ => {}
            }
        }

        assert!(saw_read_hit, "evidence must include a read cache hit");
        assert!(saw_grep_stale, "evidence must include grep stale bypass");
        assert!(saw_find_stale, "evidence must include find stale bypass");
        assert!(saw_ls_stale, "evidence must include ls stale bypass");
        assert!(saw_write_uncached, "evidence must include write uncached");
        assert!(saw_edit_uncached, "evidence must include edit uncached");
        assert!(saw_bash_uncached, "evidence must include bash uncached");
        Ok(())
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
    fn test_rg_match_rejects_missing_or_malformed_required_fields() {
        let mut matches = Vec::new();
        let mut match_count = 0usize;
        let mut match_limit_reached = false;
        let scan_limit = 1;

        let missing_line =
            Ok(r#"{"type":"match","data":{"path":{"text":"file.txt"}}}"#.to_string());
        let missing_line_error = process_rg_json_match_line(
            missing_line,
            &mut matches,
            &mut match_count,
            &mut match_limit_reached,
            scan_limit,
        )
        .expect_err("a match without line_number must fail closed");
        assert!(missing_line_error.to_string().contains("line_number"));

        let zero_line = Ok(
            r#"{"type":"match","data":{"path":{"text":"file.txt"},"line_number":0}}"#.to_string(),
        );
        let zero_line_error = process_rg_json_match_line(
            zero_line,
            &mut matches,
            &mut match_count,
            &mut match_limit_reached,
            scan_limit,
        )
        .expect_err("line_number zero must fail closed before context arithmetic");
        assert!(zero_line_error.to_string().contains("line_number"));

        let malformed_bytes =
            Ok(r#"{"type":"match","data":{"path":{"bytes":"%%%"},"line_number":2}}"#.to_string());
        let malformed_path_error = process_rg_json_match_line(
            malformed_bytes,
            &mut matches,
            &mut match_count,
            &mut match_limit_reached,
            scan_limit,
        )
        .expect_err("invalid path.bytes base64 must fail closed");
        assert!(
            malformed_path_error
                .to_string()
                .contains("invalid path.bytes base64")
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
        )
        .expect("valid match event");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].1, 3);
        assert_eq!(match_count, 1);
        assert!(match_limit_reached);
    }

    #[cfg(unix)]
    #[test]
    fn signaled_ripgrep_status_is_never_classified_as_success() {
        let status = std::process::Command::new("sh")
            .args(["-c", "kill -TERM $$"])
            .status()
            .expect("run signaled ripgrep-status fixture");
        assert!(status.code().is_none(), "fixture must terminate by signal");
        let error = rg_exit_failure(status, "").expect("signal termination must be an error");
        assert!(error.contains("signal"), "{error}");
    }

    #[test]
    fn test_truncate_by_bytes() {
        let content = "short\nthis is a longer line\nanother".to_string();
        let result = truncate_head(content, 100, 15);

        assert!(result.truncated);
        assert_eq!(result.truncated_by, Some(TruncatedBy::Bytes));
    }

    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    #[test]
    fn test_command_with_default_sigpipe_restores_pipe_disposition() {
        // Verify the spawned child does NOT inherit the parent's
        // SIGPIPE=SIG_IGN. The probe parses the SigIgn: hex mask exposed by
        // Linux-format /proc/<pid>/status — available natively on Linux and,
        // on FreeBSD, through the linprocfs compat module mounted at
        // /compat/linux/proc. Skip with a one-line notice when linprocfs is
        // not mounted rather than failing the test.
        #[cfg(target_os = "freebsd")]
        let status_dir = {
            let probe = format!("/compat/linux/proc/{}/status", std::process::id());
            if !std::path::Path::new(&probe).exists() {
                eprintln!(
                    "skipping sigpipe disposition test: linprocfs not mounted \
                     at /compat/linux/proc — add `linprocfs /compat/linux/proc \
                     linprocfs rw 0 0` to /etc/fstab and `mount /compat/linux/proc` \
                     to enable"
                );
                return;
            }
            "/compat/linux/proc"
        };
        #[cfg(not(target_os = "freebsd"))]
        let status_dir = "/proc";

        let probe_cmd = format!(
            "while read name value _; do [ \"$name\" = SigIgn: ] && \
             {{ printf '%s' \"$value\"; exit 0; }}; done < {status_dir}/$$/status"
        );

        let output = command_with_default_sigpipe("sh")
            .expect("prepare sigpipe disposition probe")
            .args(["-c", &probe_cmd])
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
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("secret.txt");
            std::fs::write(&path, "secret\n").unwrap();
            let _mode_guard = UnixModeGuard::set(&path, 0o000);

            let metadata = std::fs::metadata(&path).expect("stat unreadable fixture");
            let permission_error =
                ensure_effective_mode_access(&metadata, &path, UNIX_ACCESS_READ, "file reading")
                    .expect_err("mode invariant must reject an unreadable file");
            assert_eq!(
                permission_error.kind(),
                std::io::ErrorKind::PermissionDenied
            );

            let lines = get_file_lines_async(&path, tmp.path()).await;
            assert!(lines.is_empty());
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_get_file_lines_async_unsearchable_parent_returns_empty() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().expect("create temp dir");
            let locked_dir = tmp.path().join("locked");
            std::fs::create_dir(&locked_dir).expect("create locked directory");
            let path = locked_dir.join("secret.txt");
            std::fs::write(&path, "secret\n").expect("write secret fixture");
            let _mode_guard = UnixModeGuard::set(&locked_dir, 0o000);

            let permission_error = ensure_ancestors_searchable(&path)
                .await
                .expect_err("mode invariant must reject an unsearchable parent");
            assert_eq!(
                permission_error.kind(),
                std::io::ErrorKind::PermissionDenied
            );

            let lines = get_file_lines_async(&path, tmp.path()).await;
            assert!(lines.is_empty());
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_scoped_grep_read_rejects_terminal_symlink_swap() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().expect("create temp dir");
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("create workspace");
        let path = workspace.join("match.txt");
        let displaced = workspace.join("match-original.txt");
        std::fs::write(&path, "public match\n").expect("write original result");
        let outside = tmp.path().join("outside-secret.txt");
        std::fs::write(&outside, "outside secret\n").expect("write outside fixture");

        let result = open_scoped_regular_file_for_read_with(&path, &workspace, || {
            std::fs::rename(&path, &displaced).expect("displace original result");
            symlink(&outside, &path).expect("replace result with outside symlink");
        });

        assert!(
            result.is_err(),
            "a terminal symlink swap must never return an outside descriptor"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_scoped_grep_read_rejects_ancestor_symlink_swap() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().expect("create temp dir");
        let workspace = tmp.path().join("workspace");
        let result_parent = workspace.join("tree");
        std::fs::create_dir_all(&result_parent).expect("create result parent");
        let path = result_parent.join("match.txt");
        std::fs::write(&path, "public match\n").expect("write original result");

        let outside_parent = tmp.path().join("outside-tree");
        std::fs::create_dir(&outside_parent).expect("create outside parent");
        std::fs::write(outside_parent.join("match.txt"), "outside secret\n")
            .expect("write outside fixture");
        let displaced_parent = workspace.join("tree-original");

        let result = open_scoped_regular_file_for_read_with(&path, &workspace, || {
            std::fs::rename(&result_parent, &displaced_parent)
                .expect("displace original result parent");
            symlink(&outside_parent, &result_parent)
                .expect("replace result parent with outside symlink");
        });

        assert!(
            result.is_err(),
            "an ancestor symlink swap must never return an outside descriptor"
        );
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

    /// Issue #71: skill files, prompt templates, and themes live under the
    /// agent dir (`~/.pi/agent/`, default). The agent legitimately needs to
    /// read these even when cwd is a user project on a different path.
    /// Ensure `enforce_read_scope_with_roots` accepts the agent dir as a
    /// second valid root without breaking the cwd-only contract for paths
    /// that are under neither.
    #[test]
    fn test_enforce_read_scope_allows_agent_dir_outside_cwd() {
        let cwd = tempfile::tempdir().unwrap();
        let agent_dir = tempfile::tempdir().unwrap();
        let skill_dir = agent_dir.path().join("skills").join("freebsd-jails");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let skill_path = skill_dir.join("SKILL.md");
        std::fs::write(&skill_path, "---\nname: test\n---\n# body\n").unwrap();

        let resolved = enforce_read_scope_with_roots(
            &skill_path,
            cwd.path(),
            agent_dir.path(),
            &WorkspaceHandle::default(),
        )
        .unwrap();
        assert!(
            resolved.starts_with(
                agent_dir
                    .path()
                    .canonicalize()
                    .unwrap_or_else(|_| agent_dir.path().to_path_buf())
            ),
            "agent-dir path must be allowed and returned canonicalised"
        );
    }

    #[test]
    fn test_enforce_read_scope_still_rejects_unrelated_paths() {
        // Paths under neither cwd nor agent_dir must keep failing closed.
        let cwd = tempfile::tempdir().unwrap();
        let agent_dir = tempfile::tempdir().unwrap();
        let unrelated = tempfile::tempdir().unwrap();
        std::fs::write(unrelated.path().join("secret.txt"), "secret").unwrap();
        let secret_path = unrelated.path().join("secret.txt");

        let err = enforce_read_scope_with_roots(
            &secret_path,
            cwd.path(),
            agent_dir.path(),
            &WorkspaceHandle::default(),
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("outside the working directory") && msg.contains("agent dir"),
            "error must mention both denied roots, got: {msg}"
        );
    }

    #[test]
    fn test_enforce_read_scope_prefers_cwd_when_path_is_under_cwd() {
        // When a path is under cwd, we must not silently switch to agent-dir
        // resolution. This locks in the order of the prefix checks.
        let cwd = tempfile::tempdir().unwrap();
        let agent_dir = tempfile::tempdir().unwrap();
        std::fs::write(cwd.path().join("a.txt"), "in cwd").unwrap();

        let resolved = enforce_read_scope_with_roots(
            &cwd.path().join("a.txt"),
            cwd.path(),
            agent_dir.path(),
            &WorkspaceHandle::default(),
        )
        .unwrap();
        assert!(
            resolved.starts_with(
                cwd.path()
                    .canonicalize()
                    .unwrap_or_else(|_| cwd.path().to_path_buf())
            )
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
    fn write_tool_persists_inside_additional_root() {
        // /add-dir grants read AND write: the atomic-replace containment
        // check must accept parents under any workspace root, not only the
        // primary cwd (regression: every write/edit in an added root failed
        // with "parent escaped the working directory").
        let primary = tempfile::tempdir().unwrap();
        let extra = tempfile::tempdir().unwrap();
        std::fs::write(extra.path().join("extra.txt"), "before").unwrap();

        let mut handle = crate::workspace::WorkspaceHandle::single(primary.path());
        let canonical = crate::workspace::validate_new_root(extra.path()).unwrap();
        handle.add_root(&canonical);
        let extra_path = extra.path().join("extra.txt").to_string_lossy().to_string();

        asupersync::test_utils::run_test(|| {
            let tool = WriteTool::new(primary.path()).with_workspace(handle.clone());
            let extra_path = extra_path.clone();
            async move {
                tool.execute(
                    "t",
                    serde_json::json!({ "path": extra_path, "content": "after" }),
                    None,
                )
                .await
                .unwrap();
            }
        });
        assert_eq!(
            std::fs::read_to_string(extra.path().join("extra.txt")).unwrap(),
            "after"
        );
    }

    #[test]
    fn read_tool_spans_additional_roots_and_revokes_on_removal() {
        // Multi-root acceptance (bd-cv653.3.12): read succeeds in the
        // additional root, paths outside every root fail closed, and
        // removing the root from the shared handle revokes immediately.
        let primary = tempfile::tempdir().unwrap();
        let extra = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(extra.path().join("extra.txt"), "extra-content").unwrap();
        std::fs::write(outside.path().join("secret.txt"), "outside-content").unwrap();

        let mut handle = crate::workspace::WorkspaceHandle::single(primary.path());
        let canonical = crate::workspace::validate_new_root(extra.path()).unwrap();
        handle.add_root(&canonical);

        let extra_path = extra.path().join("extra.txt").to_string_lossy().to_string();
        let outside_path = outside
            .path()
            .join("secret.txt")
            .to_string_lossy()
            .to_string();

        // Read inside the additional root succeeds.
        asupersync::test_utils::run_test(|| {
            let tool = ReadTool::new(primary.path()).with_workspace(handle.clone());
            let extra_path = extra_path.clone();
            async move {
                let out = tool
                    .execute("t", serde_json::json!({ "path": extra_path }), None)
                    .await
                    .unwrap();
                assert!(get_text(&out.content).contains("extra-content"));
            }
        });

        // Outside every root fails closed with a named denial.
        asupersync::test_utils::run_test(|| {
            let tool = ReadTool::new(primary.path()).with_workspace(handle.clone());
            let outside_path = outside_path.clone();
            async move {
                let err = tool
                    .execute("t", serde_json::json!({ "path": outside_path }), None)
                    .await
                    .unwrap_err();
                assert!(err.to_string().contains("outside the"), "{err}");
            }
        });

        // Revoking the additional root denies the previously-allowed path.
        handle.remove_root(extra.path());
        asupersync::test_utils::run_test(|| {
            let tool = ReadTool::new(primary.path()).with_workspace(handle.clone());
            let extra_path = extra_path.clone();
            async move {
                let err = tool
                    .execute("t", serde_json::json!({ "path": extra_path }), None)
                    .await
                    .unwrap_err();
                assert!(err.to_string().contains("outside the"), "{err}");
            }
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

    /// bd-cv653.3.13: a recorder attached via the builder observes write
    /// and edit persistence, and /undo semantics restore prior content.
    #[test]
    fn test_mutation_recorder_records_write_and_edit() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            let recorder = std::sync::Arc::new(crate::undo::FileMutationRecorder::default());
            let file = tmp.path().join("tracked.txt");

            let write_tool =
                WriteTool::new(tmp.path()).with_mutation_recorder(Some(recorder.clone()));
            write_tool
                .execute(
                    "w1",
                    serde_json::json!({
                        "path": file.to_string_lossy(),
                        "content": "first\n"
                    }),
                    None,
                )
                .await
                .unwrap();

            let edit_tool =
                EditTool::new(tmp.path()).with_mutation_recorder(Some(recorder.clone()));
            edit_tool
                .execute(
                    "e1",
                    serde_json::json!({
                        "path": file.to_string_lossy(),
                        "oldText": "first",
                        "newText": "second"
                    }),
                    None,
                )
                .await
                .unwrap();

            assert_eq!(recorder.stats().undo_depth, 2);

            let outcome = recorder.undo(1, false);
            assert_eq!(outcome.applied.len(), 1, "{outcome:?}");
            assert_eq!(
                std::fs::read_to_string(&file).unwrap(),
                "first\n",
                "undo must restore the pre-edit content"
            );

            let outcome = recorder.undo(1, false);
            assert_eq!(outcome.applied.len(), 1, "{outcome:?}");
            assert!(
                !file.exists(),
                "undoing the creating write removes the file"
            );
        });
    }

    /// bd-cv653.3.13: a failed persist aborts the pending unit instead of
    /// recording a phantom mutation.
    #[test]
    fn test_mutation_recorder_skips_failed_edit() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            let recorder = std::sync::Arc::new(crate::undo::FileMutationRecorder::default());
            let file = tmp.path().join("tracked.txt");
            std::fs::write(&file, "content\n").unwrap();

            let edit_tool =
                EditTool::new(tmp.path()).with_mutation_recorder(Some(recorder.clone()));
            let result = edit_tool
                .execute(
                    "e1",
                    serde_json::json!({
                        "path": file.to_string_lossy(),
                        "oldText": "no such text",
                        "newText": "irrelevant"
                    }),
                    None,
                )
                .await;
            assert!(result.is_err(), "edit with missing oldText must fail");
            assert_eq!(recorder.stats().undo_depth, 0, "no unit for a failed edit");
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
            let cancellation = out
                .details
                .as_ref()
                .and_then(|details| details.get("cancellation"))
                .expect("timeout should include structured cancellation details");
            assert_eq!(cancellation["schema"], BASH_CANCELLATION_SCHEMA_V1);
            assert_eq!(cancellation["status"], "cancelled");
            assert_eq!(cancellation["reason"], "timeout");
            assert_eq!(cancellation["cleanup"], "process_group_tree_terminated");
            assert_eq!(cancellation["timeoutMs"], 2000);
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
            assert_eq!(
                result.cancellation_reason,
                Some(BashCancellationReason::AmbientCancellation)
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

    /// bd-cv653.1.5: the in-process and external search backends must render
    /// byte-identical tool output for the same corpus. Skips silently when
    /// the external binaries are not installed (the escape hatch is optional;
    /// the in-process default carries the suite).
    #[test]
    fn search_backends_render_identical_output() {
        asupersync::test_utils::run_test(|| async {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(tmp.path().join("src/nested")).unwrap();
            std::fs::write(
                tmp.path().join("src/alpha.txt"),
                "needle one\nplain line\nneedle two\n",
            )
            .unwrap();
            std::fs::write(tmp.path().join("src/nested/beta.txt"), "needle three\n").unwrap();
            std::fs::write(tmp.path().join("skip.log"), "needle ignored\n").unwrap();
            std::fs::write(tmp.path().join(".gitignore"), "*.log\n").unwrap();

            if rg_available() {
                let grep_input = serde_json::json!({ "pattern": "needle", "context": 1 });
                let inproc = GrepTool::with_backend(tmp.path(), SearchBackend::Inproc)
                    .execute("grep-parity-inproc", grep_input.clone(), None)
                    .await
                    .expect("inproc grep");
                reset_tool_output_cache_for_tests();
                let external = GrepTool::with_backend(tmp.path(), SearchBackend::External)
                    .execute("grep-parity-external", grep_input, None)
                    .await
                    .expect("external grep");
                // rg's multi-file result order is traversal-dependent, so
                // compare per-line content rather than byte order.
                let sorted_lines = |text: String| {
                    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
                    lines.sort();
                    lines
                };
                assert_eq!(
                    sorted_lines(get_text(&inproc.content)),
                    sorted_lines(get_text(&external.content))
                );
            }

            if find_fd_binary().is_some() {
                let find_input = serde_json::json!({ "pattern": "*.txt" });
                reset_tool_output_cache_for_tests();
                let inproc = FindTool::with_backend(tmp.path(), SearchBackend::Inproc)
                    .execute("find-parity-inproc", find_input.clone(), None)
                    .await
                    .expect("inproc find");
                reset_tool_output_cache_for_tests();
                let external = FindTool::with_backend(tmp.path(), SearchBackend::External)
                    .execute("find-parity-external", find_input, None)
                    .await
                    .expect("external find");
                assert_eq!(get_text(&inproc.content), get_text(&external.content));
            }
        });
    }

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

    #[test]
    fn hub_agent_requires_name_for_child_ops() {
        // bd-cv653.5.3: child-targeting actions must refuse without a run id.
        asupersync::test_utils::run_test(|| async {
            let tool = HubTool::new(Path::new("."));
            let input = serde_json::json!({ "op": "agent", "action": "transcript" });
            let out = tool.execute("t", input, None).await.unwrap();
            assert!(out.is_error);
            assert!(get_text(&out.content).contains("requires name"));
        });
    }

    #[test]
    fn hub_agent_unknown_child_is_named_refusal() {
        asupersync::test_utils::run_test(|| async {
            let tool = HubTool::new(Path::new("."));
            let input = serde_json::json!({
                "op": "agent",
                "action": "transcript",
                "name": "no-such-child-zzz"
            });
            let out = tool.execute("t", input, None).await.unwrap();
            assert!(out.is_error);
            assert!(get_text(&out.content).contains("unknown child"));
        });
    }

    #[test]
    fn hub_agent_steer_requires_text() {
        asupersync::test_utils::run_test(|| async {
            let tool = HubTool::new(Path::new("."));
            let input = serde_json::json!({
                "op": "agent",
                "action": "steer",
                "name": "any-child"
            });
            let out = tool.execute("t", input, None).await.unwrap();
            assert!(out.is_error);
            assert!(get_text(&out.content).contains("requires text"));
        });
    }

    #[test]
    fn hub_agent_rejects_unknown_action() {
        asupersync::test_utils::run_test(|| async {
            let tool = HubTool::new(Path::new("."));
            let input = serde_json::json!({ "op": "agent", "action": "frobnicate" });
            let out = tool.execute("t", input, None).await.unwrap();
            assert!(out.is_error);
            assert!(get_text(&out.content).contains("Unknown agent action"));
        });
    }
}
