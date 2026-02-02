//! Built-in tool implementations.
//!
//! Pi provides 7 built-in tools: read, bash, edit, write, grep, find, ls.

use crate::error::{Error, Result};
use crate::model::{ContentBlock, ImageContent, TextContent};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use uuid::Uuid;

// ============================================================================
// Tool Trait
// ============================================================================

/// A tool that can be executed by the agent.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Get the tool name.
    fn name(&self) -> &'static str;

    /// Get the tool label (display name).
    fn label(&self) -> &'static str;

    /// Get the tool description.
    fn description(&self) -> &'static str;

    /// Get the tool parameters as JSON Schema.
    fn parameters(&self) -> serde_json::Value;

    /// Execute the tool.
    async fn execute(
        &self,
        tool_call_id: &str,
        input: serde_json::Value,
        on_update: Option<Box<dyn Fn(ToolUpdate) + Send>>,
    ) -> Result<ToolOutput>;
}

/// Tool execution output.
pub struct ToolOutput {
    pub content: Vec<ContentBlock>,
    pub details: Option<serde_json::Value>,
}

/// Incremental update during tool execution.
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
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024; // 50KB

/// Maximum line length for grep results.
pub const GREP_MAX_LINE_LENGTH: usize = 500;

/// Default bash timeout in seconds.
pub const DEFAULT_BASH_TIMEOUT: u64 = 120;

/// Default grep result limit.
pub const DEFAULT_GREP_LIMIT: usize = 1000;

/// Default find result limit.
pub const DEFAULT_FIND_LIMIT: usize = 1000;

/// Default ls result limit.
pub const DEFAULT_LS_LIMIT: usize = 1000;

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
pub fn truncate_head(content: &str, max_lines: usize, max_bytes: usize) -> TruncationResult {
    let total_bytes = content.len();
    let lines: Vec<&str> = content.split('\n').collect();
    let total_lines = lines.len();

    // No truncation needed
    if total_lines <= max_lines && total_bytes <= max_bytes {
        return TruncationResult {
            content: content.to_string(),
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

    // If the first line alone exceeds the byte limit, return empty content.
    let first_line_bytes = lines.first().map_or(0, |l| l.len());
    if first_line_bytes > max_bytes {
        return TruncationResult {
            content: String::new(),
            truncated: true,
            truncated_by: Some(TruncatedBy::Bytes),
            total_lines,
            total_bytes,
            output_lines: 0,
            output_bytes: 0,
            last_line_partial: false,
            first_line_exceeds_limit: true,
            max_lines,
            max_bytes,
        };
    }

    let mut output = String::new();
    let mut line_count = 0;
    let mut byte_count: usize = 0;
    let mut truncated_by = None;

    for (i, line) in lines.iter().enumerate() {
        if i >= max_lines {
            truncated_by = Some(TruncatedBy::Lines);
            break;
        }

        let line_bytes = line.len() + usize::from(i > 0); // +1 for newline
        if line_count >= max_lines {
            truncated_by = Some(TruncatedBy::Lines);
            break;
        }

        if byte_count + line_bytes > max_bytes {
            truncated_by = Some(TruncatedBy::Bytes);
            break;
        }

        if i > 0 {
            output.push('\n');
        }
        output.push_str(line);
        line_count += 1;
        byte_count += line_bytes;
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
        last_line_partial: false,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    }
}

/// Truncate from the end (keep last N lines).
pub fn truncate_tail(content: &str, max_lines: usize, max_bytes: usize) -> TruncationResult {
    let total_bytes = content.len();
    let lines: Vec<&str> = content.split('\n').collect();
    let total_lines = lines.len();

    // No truncation needed
    if total_lines <= max_lines && total_bytes <= max_bytes {
        return TruncationResult {
            content: content.to_string(),
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

    let mut output_lines = Vec::new();
    let mut byte_count: usize = 0;
    let mut truncated_by = None;
    let mut last_line_partial = false;

    // Iterate from the end
    for line in lines.iter().rev() {
        let line_bytes = line.len() + usize::from(!output_lines.is_empty());

        if output_lines.len() >= max_lines {
            truncated_by = Some(TruncatedBy::Lines);
            break;
        }

        if byte_count + line_bytes > max_bytes {
            // Check if we can include a partial last line
            let remaining = max_bytes.saturating_sub(byte_count);
            if remaining > 0 && output_lines.is_empty() {
                output_lines.push(truncate_string_to_bytes_from_end(line, max_bytes));
                last_line_partial = true;
            }
            truncated_by = Some(TruncatedBy::Bytes);
            break;
        }

        output_lines.push((*line).to_string());
        byte_count += line_bytes;
    }

    output_lines.reverse();
    let output = output_lines.join("\n");
    let output_bytes = output.len();

    TruncationResult {
        content: output,
        truncated: truncated_by.is_some(),
        truncated_by,
        total_lines,
        total_bytes,
        output_lines: output_lines.len(),
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
        ['\u{2010}', '\u{2011}', '\u{2012}', '\u{2013}', '\u{2014}', '\u{2015}', '\u{2212}'],
        "-",
    )
}

fn normalize_for_match(s: &str) -> String {
    let s = normalize_unicode_spaces(s);
    let s = normalize_quotes(&s);
    normalize_dashes(&s)
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
fn resolve_read_path(file_path: &str, cwd: &Path) -> PathBuf {
    let resolved = resolve_to_cwd(file_path, cwd);
    if file_exists(&resolved) {
        return resolved;
    }

    let Some(resolved_str) = resolved.to_str() else {
        return resolved;
    };

    let am_pm_variant = try_mac_os_screenshot_path(resolved_str);
    if am_pm_variant != resolved_str && file_exists(Path::new(&am_pm_variant)) {
        return PathBuf::from(am_pm_variant);
    }

    let nfd_variant = try_nfd_variant(resolved_str);
    if nfd_variant != resolved_str && file_exists(Path::new(&nfd_variant)) {
        return PathBuf::from(nfd_variant);
    }

    let curly_variant = try_curly_quote_variant(resolved_str);
    if curly_variant != resolved_str && file_exists(Path::new(&curly_variant)) {
        return PathBuf::from(curly_variant);
    }

    let nfd_curly_variant = try_curly_quote_variant(&nfd_variant);
    if nfd_curly_variant != resolved_str && file_exists(Path::new(&nfd_curly_variant)) {
        return PathBuf::from(nfd_curly_variant);
    }

    resolved
}

/// Resolve a file path relative to the current working directory.
/// Public alias for `resolve_to_cwd` used by tools.
fn resolve_path(file_path: &str, cwd: &Path) -> PathBuf {
    resolve_to_cwd(file_path, cwd)
}

/// Check if a file is an image based on extension.
fn is_image_file(path: &Path) -> bool {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    matches!(
        ext.to_lowercase().as_str(),
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "ico" | "tiff" | "tif"
    )
}

/// Get the MIME type for an image file based on extension.
fn image_mime_type(path: &Path) -> &'static str {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    match ext.to_lowercase().as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        "tiff" | "tif" => "image/tiff",
        _ => "application/octet-stream",
    }
}

/// Add line numbers to content (cat -n style).
fn add_line_numbers(content: &str, start_line: usize) -> String {
    content
        .lines()
        .enumerate()
        .map(|(i, line)| format!("{}\t{}", start_line + i, line))
        .collect::<Vec<_>>()
        .join("\n")
}

// ============================================================================
// Tool Registry
// ============================================================================

/// Registry of available tools.
pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
}

impl ToolRegistry {
    /// Create a new registry with the specified tools enabled.
    pub fn new(enabled: &[&str], cwd: &Path) -> Self {
        let mut tools: Vec<Box<dyn Tool>> = Vec::new();

        for name in enabled {
            match *name {
                "read" => tools.push(Box::new(ReadTool::new(cwd))),
                "bash" => tools.push(Box::new(BashTool::new(cwd))),
                "edit" => tools.push(Box::new(EditTool::new(cwd))),
                "write" => tools.push(Box::new(WriteTool::new(cwd))),
                "grep" => tools.push(Box::new(GrepTool::new(cwd))),
                "find" => tools.push(Box::new(FindTool::new(cwd))),
                "ls" => tools.push(Box::new(LsTool::new(cwd))),
                _ => {}
            }
        }

        Self { tools }
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
    offset: Option<usize>,
    limit: Option<usize>,
}

pub struct ReadTool {
    cwd: PathBuf,
}

impl ReadTool {
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
        }
    }
}

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &'static str {
        "read"
    }
    fn label(&self) -> &'static str {
        "Read File"
    }
    fn description(&self) -> &'static str {
        "Read file contents with optional line offset and limit"
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path (absolute, relative, or ~-prefixed)"
                },
                "offset": {
                    "type": "integer",
                    "description": "Line to start from (1-indexed, default: 1)",
                    "minimum": 1
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum lines to read (default: 2000)",
                    "minimum": 1
                }
            },
            "required": ["path"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send>>,
    ) -> Result<ToolOutput> {
        let input: ReadInput =
            serde_json::from_value(input).map_err(|e| Error::validation(e.to_string()))?;

        let path = resolve_path(&input.path, &self.cwd);
        let offset = input.offset.unwrap_or(1).max(1);
        let limit = input.limit.unwrap_or(DEFAULT_MAX_LINES);

        // Check if file exists
        if !path.exists() {
            return Err(Error::tool(
                "read",
                format!("File not found: {}", path.display()),
            ));
        }

        // Check if it's a directory
        if path.is_dir() {
            return Err(Error::tool(
                "read",
                format!("Path is a directory: {}", path.display()),
            ));
        }

        // Handle image files
        if is_image_file(&path) {
            let data = tokio::fs::read(&path)
                .await
                .map_err(|e| Error::tool("read", e.to_string()))?;
            let base64_data =
                base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &data);
            let mime_type = image_mime_type(&path);

            return Ok(ToolOutput {
                content: vec![ContentBlock::Image(ImageContent {
                    data: base64_data,
                    mime_type: mime_type.to_string(),
                })],
                details: Some(serde_json::json!({
                    "path": path.display().to_string(),
                    "size": data.len(),
                    "mimeType": mime_type,
                })),
            });
        }

        // Read text file
        let content = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| Error::tool("read", format!("Failed to read file: {e}")))?;

        // Apply offset and limit
        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len();
        let start_idx = (offset - 1).min(total_lines);
        let end_idx = (start_idx + limit).min(total_lines);
        let selected_lines: Vec<&str> = lines[start_idx..end_idx].to_vec();
        let selected_content = selected_lines.join("\n");

        // Apply truncation (by bytes)
        let result = truncate_head(&selected_content, limit, DEFAULT_MAX_BYTES);

        // Add line numbers
        let numbered_content = add_line_numbers(&result.content, offset);

        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(numbered_content))],
            details: Some(serde_json::json!({
                "path": path.display().to_string(),
                "totalLines": total_lines,
                "offset": offset,
                "limit": limit,
                "outputLines": result.output_lines,
                "truncated": result.truncated,
                "truncatedBy": result.truncated_by,
            })),
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
}

impl BashTool {
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
        }
    }
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &'static str {
        "bash"
    }
    fn label(&self) -> &'static str {
        "Bash"
    }
    fn description(&self) -> &'static str {
        "Execute bash commands"
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
                    "description": "Timeout in seconds (default: 120)",
                    "minimum": 1
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
        on_update: Option<Box<dyn Fn(ToolUpdate) + Send>>,
    ) -> Result<ToolOutput> {
        let input: BashInput =
            serde_json::from_value(input).map_err(|e| Error::validation(e.to_string()))?;

        let timeout_secs = input.timeout.unwrap_or(DEFAULT_BASH_TIMEOUT);

        // Spawn the bash process
        let mut child = Command::new("bash")
            .arg("-c")
            .arg(&input.command)
            .current_dir(&self.cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| Error::tool("bash", format!("Failed to spawn bash: {e}")))?;

        let stdout = child.stdout.take().expect("stdout should be piped");
        let stderr = child.stderr.take().expect("stderr should be piped");

        let mut stdout_reader = BufReader::new(stdout).lines();
        let mut stderr_reader = BufReader::new(stderr).lines();

        let mut output = String::new();
        let mut rolling_output = String::new();
        let mut exit_code: Option<i32> = None;
        let mut timed_out = false;

        let timeout = tokio::time::Duration::from_secs(timeout_secs);
        let start = tokio::time::Instant::now();

        loop {
            if start.elapsed() > timeout {
                timed_out = true;
                let _ = child.kill().await;
                break;
            }

            tokio::select! {
                line = stdout_reader.next_line() => {
                    if let Ok(Some(line)) = line {
                        append_output(&mut output, &mut rolling_output, &line);
                        if let Some(ref callback) = on_update {
                            callback(ToolUpdate {
                                content: vec![ContentBlock::Text(TextContent::new(rolling_output.clone()))],
                                details: None,
                            });
                        }
                    }
                }
                line = stderr_reader.next_line() => {
                    if let Ok(Some(line)) = line {
                        append_output(&mut output, &mut rolling_output, &line);
                        if let Some(ref callback) = on_update {
                            callback(ToolUpdate {
                                content: vec![ContentBlock::Text(TextContent::new(rolling_output.clone()))],
                                details: None,
                            });
                        }
                    }
                }
                status = child.wait() => {
                    exit_code = status
                        .map_err(|e| Error::tool("bash", e.to_string()))?
                        .code();
                    break;
                }
            }
        }

        Ok(build_bash_output(&output, exit_code, timed_out, timeout_secs))
    }
}

const BASH_ROLLING_BUFFER_BYTES: usize = 100 * 1024;

fn append_output(full: &mut String, rolling: &mut String, line: &str) {
    if !full.is_empty() {
        full.push('\n');
    }
    full.push_str(line);

    if !rolling.is_empty() {
        rolling.push('\n');
    }
    rolling.push_str(line);

    if rolling.len() > BASH_ROLLING_BUFFER_BYTES {
        *rolling = truncate_string_to_bytes_from_end(rolling, BASH_ROLLING_BUFFER_BYTES);
    }
}

fn build_bash_output(
    full_output: &str,
    exit_code: Option<i32>,
    timed_out: bool,
    timeout_secs: u64,
) -> ToolOutput {
    let truncation = truncate_tail(full_output, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
    let mut output_text = if truncation.content.is_empty() {
        "(no output)".to_string()
    } else {
        truncation.content.clone()
    };

    let mut details = serde_json::Map::new();
    details.insert(
        "exitCode".to_string(),
        exit_code.map_or(serde_json::Value::Null, |c| {
            serde_json::Value::Number(c.into())
        }),
    );
    details.insert(
        "timedOut".to_string(),
        serde_json::Value::Bool(timed_out),
    );
    details.insert(
        "timeout".to_string(),
        serde_json::Value::Number(timeout_secs.into()),
    );
    details.insert(
        "totalLines".to_string(),
        serde_json::Value::Number(truncation.total_lines.into()),
    );
    details.insert(
        "outputLines".to_string(),
        serde_json::Value::Number(truncation.output_lines.into()),
    );
    details.insert(
        "truncated".to_string(),
        serde_json::Value::Bool(truncation.truncated),
    );

    let is_error = timed_out || exit_code.is_some_and(|c| c != 0);
    details.insert("isError".to_string(), serde_json::Value::Bool(is_error));

    if truncation.truncated {
        if let Ok(path) = write_full_output(full_output) {
            details.insert(
                "fullOutputPath".to_string(),
                serde_json::Value::String(path.display().to_string()),
            );

            let start_line = truncation
                .total_lines
                .saturating_sub(truncation.output_lines)
                .saturating_add(1);
            let end_line = truncation.total_lines;

            if truncation.last_line_partial {
                let last_line_bytes = full_output
                    .split('\n')
                    .next_back()
                    .map_or(0, str::len);
                let _ = write!(
                    output_text,
                    "\n\n[Showing last {} of line {end_line} (line is {}). Full output: {}]",
                    format_size(truncation.output_bytes),
                    format_size(last_line_bytes),
                    path.display()
                );
            } else if truncation.truncated_by == Some(TruncatedBy::Lines) {
                let _ = write!(
                    output_text,
                    "\n\n[Showing lines {start_line}-{end_line} of {}. Full output: {}]",
                    truncation.total_lines,
                    path.display()
                );
            } else {
                let _ = write!(
                    output_text,
                    "\n\n[Showing lines {start_line}-{end_line} of {} ({} limit). Full output: {}]",
                    truncation.total_lines,
                    format_size(DEFAULT_MAX_BYTES),
                    path.display()
                );
            }
        }
    }

    if timed_out {
        let _ = write!(
            output_text,
            "\n\nCommand timed out after {timeout_secs} seconds"
        );
    } else if let Some(code) = exit_code {
        if code != 0 {
            let _ = write!(output_text, "\n\nCommand exited with code {code}");
        }
    }

    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(output_text))],
        details: Some(serde_json::Value::Object(details)),
    }
}

fn write_full_output(output: &str) -> Result<PathBuf> {
    let mut file = tempfile::NamedTempFile::new().map_err(|e| Error::tool("bash", e.to_string()))?;
    use std::io::Write;
    file.write_all(output.as_bytes())
        .map_err(|e| Error::tool("bash", e.to_string()))?;
    let (_file, path) = file.keep().map_err(|e| Error::tool("bash", e.to_string()))?;
    Ok(path)
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

/// Compute a simple unified diff.
fn compute_diff(old: &str, new: &str, path: &str) -> String {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();

    let mut diff = format!("--- a/{path}\n+++ b/{path}\n");

    // Simple line-by-line diff (not optimal, but works for small edits)
    let mut i = 0;
    let mut j = 0;

    while i < old_lines.len() || j < new_lines.len() {
        if i < old_lines.len() && j < new_lines.len() && old_lines[i] == new_lines[j] {
            i += 1;
            j += 1;
        } else {
            // Find the extent of the change
            let old_start = i;
            let new_start = j;

            // Skip differing lines
            while i < old_lines.len()
                && (j >= new_lines.len() || old_lines[i] != *new_lines.get(j).unwrap_or(&""))
            {
                i += 1;
            }
            while j < new_lines.len()
                && (i >= old_lines.len() || new_lines[j] != *old_lines.get(i).unwrap_or(&""))
            {
                j += 1;
            }

            // Output the hunk
            let old_count = i - old_start;
            let new_count = j - new_start;

            let _ = writeln!(
                diff,
                "@@ -{},{} +{},{} @@",
                old_start + 1,
                old_count,
                new_start + 1,
                new_count
            );

            for line in &old_lines[old_start..i] {
                let _ = writeln!(diff, "-{line}");
            }
            for line in &new_lines[new_start..j] {
                let _ = writeln!(diff, "+{line}");
            }
        }
    }

    diff
}

fn strip_bom(s: &str) -> (String, bool) {
    if let Some(stripped) = s.strip_prefix('\u{FEFF}') {
        (stripped.to_string(), true)
    } else {
        (s.to_string(), false)
    }
}

fn detect_line_ending(s: &str) -> &str {
    if s.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

fn line_offsets(lines: &[&str]) -> Vec<usize> {
    let mut offsets = Vec::with_capacity(lines.len() + 1);
    let mut pos = 0usize;
    for line in lines {
        offsets.push(pos);
        pos += line.len() + 1; // +1 for '\n'
    }
    offsets.push(pos);
    offsets
}

fn find_exact_match(content: &str, old_text: &str) -> Result<(usize, usize, usize)> {
    let mut occurrences = 0;
    let mut first_start = 0;
    for (idx, _) in content.match_indices(old_text) {
        occurrences += 1;
        if occurrences == 1 {
            first_start = idx;
        }
    }

    if occurrences == 0 {
        return Err(Error::tool(
            "edit",
            "Text not found in file. Make sure the oldText matches exactly, including whitespace."
                .to_string(),
        ));
    }
    if occurrences > 1 {
        return Err(Error::tool(
            "edit",
            format!(
                "Found {occurrences} occurrences of the text. Please provide more context to make the match unique."
            ),
        ));
    }

    let start = first_start;
    let end = first_start + old_text.len();
    let first_line = content[..start].lines().count() + 1;
    Ok((start, end, first_line))
}

fn find_fuzzy_match(content: &str, old_text: &str) -> Result<(usize, usize, usize)> {
    let content_lines: Vec<&str> = content.split('\n').collect();
    let old_lines: Vec<&str> = old_text.split('\n').collect();

    if old_lines.is_empty() {
        return Err(Error::tool(
            "edit",
            "Old text is empty".to_string(),
        ));
    }

    let normalized_content: Vec<String> = content_lines
        .iter()
        .map(|l| normalize_line_for_match(l))
        .collect();
    let normalized_old: Vec<String> = old_lines
        .iter()
        .map(|l| normalize_line_for_match(l))
        .collect();

    let mut match_start: Option<usize> = None;
    if content_lines.len() >= old_lines.len() {
        for i in 0..=content_lines.len() - old_lines.len() {
            if normalized_content[i..i + old_lines.len()] == normalized_old[..] {
                if match_start.is_some() {
                    return Err(Error::tool(
                        "edit",
                        "Multiple fuzzy matches found. Please provide more context to make the match unique."
                            .to_string(),
                    ));
                }
                match_start = Some(i);
            }
        }
    }

    let Some(start_line) = match_start else {
        return Err(Error::tool(
            "edit",
            "Text not found in file. Make sure the oldText matches exactly, including whitespace."
                .to_string(),
        ));
    };

    let offsets = line_offsets(&content_lines);
    let start = offsets[start_line];
    let end = offsets[start_line + old_lines.len()];
    let first_line = start_line + 1;
    Ok((start, end, first_line))
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &'static str {
        "edit"
    }
    fn label(&self) -> &'static str {
        "Edit"
    }
    fn description(&self) -> &'static str {
        "Replace exact text in a file"
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path to edit"
                },
                "oldText": {
                    "type": "string",
                    "description": "Exact text to find and replace"
                },
                "newText": {
                    "type": "string",
                    "description": "Replacement text"
                }
            },
            "required": ["path", "oldText", "newText"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send>>,
    ) -> Result<ToolOutput> {
        let input: EditInput =
            serde_json::from_value(input).map_err(|e| Error::validation(e.to_string()))?;

        let path = resolve_path(&input.path, &self.cwd);

        // Check if file exists
        if !path.exists() {
            return Err(Error::tool(
                "edit",
                format!("File not found: {}", path.display()),
            ));
        }

        // Read the file as bytes to preserve BOM and line endings.
        let raw = tokio::fs::read(&path)
            .await
            .map_err(|e| Error::tool("edit", format!("Failed to read file: {e}")))?;
        let text = String::from_utf8_lossy(&raw).to_string();
        let (content_no_bom, had_bom) = strip_bom(&text);
        let line_ending = detect_line_ending(&content_no_bom);
        let content_lf = content_no_bom.replace("\r\n", "\n");
        let old_lf = input.old_text.replace("\r\n", "\n");
        let new_lf = input.new_text.replace("\r\n", "\n");

        if old_lf == new_lf {
            return Err(Error::tool(
                "edit",
                "oldText and newText are identical. No changes to apply.".to_string(),
            ));
        }

        // Try exact match first.
        let match_result = find_exact_match(&content_lf, &old_lf)
            .or_else(|_| find_fuzzy_match(&content_lf, &old_lf))?;

        let (start, end, first_line) = match_result;

        let mut new_content_lf = String::new();
        new_content_lf.push_str(&content_lf[..start]);
        new_content_lf.push_str(&new_lf);
        new_content_lf.push_str(&content_lf[end..]);

        // Compute diff (LF normalized)
        let diff = compute_diff(&content_lf, &new_content_lf, &input.path);

        // Write atomically using tempfile
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let temp_file = tempfile::NamedTempFile::new_in(parent)
            .map_err(|e| Error::tool("edit", format!("Failed to create temp file: {e}")))?;

        let mut output_content = if line_ending == "\r\n" {
            new_content_lf.replace("\n", "\r\n")
        } else {
            new_content_lf.clone()
        };
        if had_bom {
            output_content = format!("\u{FEFF}{output_content}");
        }

        tokio::fs::write(temp_file.path(), &output_content)
            .await
            .map_err(|e| Error::tool("edit", format!("Failed to write temp file: {e}")))?;

        // Persist (atomic rename)
        temp_file
            .persist(&path)
            .map_err(|e| Error::tool("edit", format!("Failed to persist file: {e}")))?;

        let summary = format!(
            "Successfully replaced text in {}. Changed {} characters to {} characters.",
            path.display(),
            input.old_text.len(),
            input.new_text.len()
        );

        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(summary))],
            details: Some(serde_json::json!({
                "path": path.display().to_string(),
                "oldLength": input.old_text.len(),
                "newLength": input.new_text.len(),
                "firstLine": first_line,
                "diff": diff,
            })),
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
impl Tool for WriteTool {
    fn name(&self) -> &'static str {
        "write"
    }
    fn label(&self) -> &'static str {
        "Write"
    }
    fn description(&self) -> &'static str {
        "Write content to a file"
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path to write"
                },
                "content": {
                    "type": "string",
                    "description": "Content to write to the file"
                }
            },
            "required": ["path", "content"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send>>,
    ) -> Result<ToolOutput> {
        let input: WriteInput =
            serde_json::from_value(input).map_err(|e| Error::validation(e.to_string()))?;

        let path = resolve_path(&input.path, &self.cwd);

        // Create parent directories if needed
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| Error::tool("write", format!("Failed to create directories: {e}")))?;
        }

        let bytes_written = input.content.len();

        // Write atomically using tempfile
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let temp_file = tempfile::NamedTempFile::new_in(parent)
            .map_err(|e| Error::tool("write", format!("Failed to create temp file: {e}")))?;

        tokio::fs::write(temp_file.path(), &input.content)
            .await
            .map_err(|e| Error::tool("write", format!("Failed to write temp file: {e}")))?;

        // Persist (atomic rename)
        temp_file
            .persist(&path)
            .map_err(|e| Error::tool("write", format!("Failed to persist file: {e}")))?;

        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(format!(
                "Successfully wrote {} bytes to {}",
                bytes_written,
                path.display()
            )))],
            details: Some(serde_json::json!({
                "path": path.display().to_string(),
                "bytesWritten": bytes_written,
                "lines": input.content.lines().count(),
            })),
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

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &'static str {
        "grep"
    }
    fn label(&self) -> &'static str {
        "Grep"
    }
    fn description(&self) -> &'static str {
        "Search file contents using regex patterns"
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Regex pattern to search for"
                },
                "path": {
                    "type": "string",
                    "description": "Directory or file to search (default: current directory)"
                },
                "glob": {
                    "type": "string",
                    "description": "Glob pattern to filter files (e.g., '*.rs')"
                },
                "ignoreCase": {
                    "type": "boolean",
                    "description": "Case-insensitive search"
                },
                "literal": {
                    "type": "boolean",
                    "description": "Treat pattern as literal string, not regex"
                },
                "context": {
                    "type": "integer",
                    "description": "Number of context lines before and after matches"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of matches to return"
                }
            },
            "required": ["pattern"]
        })
    }

    #[allow(clippy::too_many_lines)]
    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send>>,
    ) -> Result<ToolOutput> {
        let input: GrepInput =
            serde_json::from_value(input).map_err(|e| Error::validation(e.to_string()))?;

        let search_dir = input.path.as_deref().unwrap_or(".");
        let search_path = resolve_path(search_dir, &self.cwd);

        let is_directory = std::fs::metadata(&search_path)
            .map_err(|_| Error::tool("grep", format!("Path not found: {}", search_path.display())))?
            .is_dir();

        let context_value = input.context.unwrap_or(0);
        let effective_limit = input.limit.unwrap_or(DEFAULT_GREP_LIMIT).max(1);

        let mut args: Vec<String> = vec![
            "--json".to_string(),
            "--line-number".to_string(),
            "--color=never".to_string(),
            "--hidden".to_string(),
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

        args.push(input.pattern.clone());
        args.push(search_path.display().to_string());

        let mut child = Command::new("rg")
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| Error::tool("grep", format!("Failed to run ripgrep: {}", e)))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::tool("grep", "Missing stdout".to_string()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| Error::tool("grep", "Missing stderr".to_string()))?;

        let mut stdout_lines = BufReader::new(stdout).lines();
        let stderr_task = tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut buf = Vec::new();
            let _ = reader.read_to_end(&mut buf).await;
            buf
        });

        let mut matches: Vec<(PathBuf, usize)> = Vec::new();
        let mut match_count: usize = 0;
        let mut match_limit_reached = false;

        while let Some(line) = stdout_lines
            .next_line()
            .await
            .map_err(|e| Error::tool("grep", e.to_string()))?
        {
            if match_count >= effective_limit {
                break;
            }

            if line.trim().is_empty() {
                continue;
            }

            let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };

            if event.get("type").and_then(serde_json::Value::as_str) != Some("match") {
                continue;
            }

            match_count += 1;

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
            }

            if match_count >= effective_limit {
                match_limit_reached = true;
                let _ = child.kill().await;
                break;
            }
        }

        let status = child
            .wait()
            .await
            .map_err(|e| Error::tool("grep", e.to_string()))?;

        let stderr_bytes = stderr_task.await.unwrap_or_default();
        let stderr_text = String::from_utf8_lossy(&stderr_bytes).trim().to_string();
        let code = status.code().unwrap_or(0);

        if !match_limit_reached && code != 0 && code != 1 {
            let msg = if stderr_text.is_empty() {
                format!("ripgrep exited with code {code}")
            } else {
                stderr_text
            };
            return Err(Error::tool("grep", msg));
        }

        if match_count == 0 {
            return Ok(ToolOutput {
                content: vec![ContentBlock::Text(TextContent::new("No matches found"))],
                details: None,
            });
        }

        let mut file_cache: HashMap<PathBuf, Vec<String>> = HashMap::new();
        let mut output_lines: Vec<String> = Vec::new();
        let mut lines_truncated = false;

        for (file_path, line_number) in &matches {
            let relative_path = format_grep_path(file_path, &search_path, is_directory);
            let lines = get_file_lines(file_path, &mut file_cache);

            if lines.is_empty() {
                output_lines.push(format!(
                    "{relative_path}:{line_number}: (unable to read file)"
                ));
                continue;
            }

            let start = if context_value > 0 {
                line_number.saturating_sub(context_value).max(1)
            } else {
                *line_number
            };
            let end = if context_value > 0 {
                (line_number + context_value).min(lines.len())
            } else {
                *line_number
            };

            for current in start..=end {
                let line_text = lines.get(current - 1).map(String::as_str).unwrap_or("");
                let sanitized = line_text.replace('\r', "");
                let truncated = truncate_line(&sanitized, GREP_MAX_LINE_LENGTH);
                if truncated.was_truncated {
                    lines_truncated = true;
                }

                if current == *line_number {
                    output_lines.push(format!("{relative_path}:{current}: {}", truncated.text));
                } else {
                    output_lines.push(format!("{relative_path}-{current}- {}", truncated.text));
                }
            }
        }

        // Apply byte truncation (no line limit since we already have match limit).
        let raw_output = output_lines.join("\n");
        let truncation = truncate_head(&raw_output, usize::MAX, DEFAULT_MAX_BYTES);

        let mut output = truncation.content.clone();
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
            output.push_str(&format!("\n\n[{}]", notices.join(". ")));
        }

        let details = if details_map.is_empty() {
            None
        } else {
            Some(serde_json::Value::Object(details_map))
        };

        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(output))],
            details,
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
impl Tool for FindTool {
    fn name(&self) -> &'static str {
        "find"
    }
    fn label(&self) -> &'static str {
        "Find"
    }
    fn description(&self) -> &'static str {
        "Find files by glob pattern"
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Glob pattern to match files (e.g., '**/*.rs', 'src/*.ts')"
                },
                "path": {
                    "type": "string",
                    "description": "Directory to search (default: current directory)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of results (default: 1000)"
                }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send>>,
    ) -> Result<ToolOutput> {
        let input: FindInput =
            serde_json::from_value(input).map_err(|e| Error::validation(e.to_string()))?;

        let search_dir = input.path.as_deref().unwrap_or(".");
        let search_path = resolve_path(search_dir, &self.cwd);
        let effective_limit = input.limit.unwrap_or(DEFAULT_FIND_LIMIT);

        let fd_cmd = find_fd_binary().ok_or_else(|| {
            Error::tool("find", "fd is not available and could not be found in PATH".to_string())
        })?;

        // Build fd arguments
        let mut args: Vec<String> = vec![
            "--glob".to_string(),
            "--color=never".to_string(),
            "--hidden".to_string(),
            "--max-results".to_string(),
            effective_limit.to_string(),
        ];

        // Include root .gitignore and nested .gitignore files (excluding node_modules/.git).
        let mut gitignore_files: Vec<PathBuf> = Vec::new();
        let root_gitignore = search_path.join(".gitignore");
        if root_gitignore.exists() {
            gitignore_files.push(root_gitignore);
        }

        let nested_pattern = search_path.join("**/.gitignore");
        if let Some(pattern_str) = nested_pattern.to_str() {
            if let Ok(paths) = glob::glob(pattern_str) {
                for entry in paths.flatten() {
                    let entry_str = entry.to_string_lossy();
                    if entry_str.contains("node_modules") || entry_str.contains("/.git/") {
                        continue;
                    }
                    gitignore_files.push(entry);
                }
            }
        }

        gitignore_files.sort();
        gitignore_files.dedup();

        for gi in gitignore_files {
            args.push("--ignore-file".to_string());
            args.push(gi.display().to_string());
        }

        args.push(input.pattern.clone());
        args.push(search_path.display().to_string());

        let output = Command::new(fd_cmd)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| Error::tool("find", format!("Failed to run fd: {e}")))?;

        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

        if !output.status.success() && stdout.is_empty() {
            let code = output.status.code().unwrap_or(1);
            let msg = if stderr.is_empty() {
                format!("fd exited with code {code}")
            } else {
                stderr
            };
            return Err(Error::tool("find", msg));
        }

        if stdout.is_empty() {
            return Ok(ToolOutput {
                content: vec![ContentBlock::Text(TextContent::new(
                    "No files found matching pattern",
                ))],
                details: None,
            });
        }

        let search_path_str = search_path.display().to_string();
        let mut relativized: Vec<String> = Vec::new();
        for raw_line in stdout.lines() {
            let line = raw_line.trim_end_matches('\r').trim();
            if line.is_empty() {
                continue;
            }

            let had_trailing_slash = line.ends_with('/') || line.ends_with('\\');
            let mut rel = if Path::new(line).is_absolute() && line.starts_with(&search_path_str) {
                line[search_path_str.len()..]
                    .trim_start_matches(['/', '\\'])
                    .to_string()
            } else {
                line.to_string()
            };

            if had_trailing_slash && !rel.ends_with('/') {
                rel.push('/');
            }

            relativized.push(rel);
        }

        if relativized.is_empty() {
            return Ok(ToolOutput {
                content: vec![ContentBlock::Text(TextContent::new(
                    "No files found matching pattern",
                ))],
                details: None,
            });
        }

        let result_limit_reached = relativized.len() >= effective_limit;
        let raw_output = relativized.join("\n");
        let truncation = truncate_head(&raw_output, usize::MAX, DEFAULT_MAX_BYTES);

        let mut result_output = truncation.content.clone();
        let mut notices: Vec<String> = Vec::new();
        let mut details_map = serde_json::Map::new();

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
            result_output.push_str(&format!("\n\n[{}]", notices.join(". ")));
        }

        let details = if details_map.is_empty() {
            None
        } else {
            Some(serde_json::Value::Object(details_map))
        };

        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(result_output))],
            details,
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
impl Tool for LsTool {
    fn name(&self) -> &'static str {
        "ls"
    }
    fn label(&self) -> &'static str {
        "List"
    }
    fn description(&self) -> &'static str {
        "List directory contents"
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

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send>>,
    ) -> Result<ToolOutput> {
        let input: LsInput =
            serde_json::from_value(input).map_err(|e| Error::validation(e.to_string()))?;

        let dir_path = input
            .path
            .as_ref()
            .map_or_else(|| self.cwd.clone(), |p| resolve_path(p, &self.cwd));

        let effective_limit = input.limit.unwrap_or(DEFAULT_LS_LIMIT);

        if !dir_path.exists() {
            return Err(Error::tool("ls", format!("Path not found: {}", dir_path.display())));
        }
        if !dir_path.is_dir() {
            return Err(Error::tool(
                "ls",
                format!("Not a directory: {}", dir_path.display()),
            ));
        }

        let mut entries = Vec::new();
        let mut read_dir = tokio::fs::read_dir(&dir_path)
            .await
            .map_err(|e| Error::tool("ls", format!("Cannot read directory: {e}")))?;

        while let Some(entry) = read_dir
            .next_entry()
            .await
            .map_err(|e| Error::tool("ls", format!("Cannot read directory: {e}")))?
        {
            entries.push(entry.file_name().to_string_lossy().to_string());
        }

        // Sort alphabetically (case-insensitive).
        entries.sort_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()));

        let mut results: Vec<String> = Vec::new();
        let mut entry_limit_reached = false;

        for entry in entries {
            if results.len() >= effective_limit {
                entry_limit_reached = true;
                break;
            }

            let full_path = dir_path.join(&entry);
            let Ok(meta) = tokio::fs::metadata(&full_path).await else {
                // Skip entries we can't stat.
                continue;
            };

            if meta.is_dir() {
                results.push(format!("{entry}/"));
            } else {
                results.push(entry);
            }
        }

        if results.is_empty() {
            return Ok(ToolOutput {
                content: vec![ContentBlock::Text(TextContent::new("(empty directory)"))],
                details: None,
            });
        }

        // Apply byte truncation (no line limit since we already have entry limit).
        let raw_output = results.join("\n");
        let truncation = truncate_head(&raw_output, usize::MAX, DEFAULT_MAX_BYTES);

        let mut output = truncation.content.clone();
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

        if truncation.truncated {
            notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
            details_map.insert("truncation".to_string(), serde_json::to_value(truncation)?);
        }

        if !notices.is_empty() {
            output.push_str(&format!("\n\n[{}]", notices.join(". ")));
        }

        let details = if details_map.is_empty() {
            None
        } else {
            Some(serde_json::Value::Object(details_map))
        };

        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(output))],
            details,
        })
    }
}

// ============================================================================
// Helper functions
// ============================================================================

async fn pump_stream<R: tokio::io::AsyncRead + Unpin + Send + 'static>(
    mut reader: R,
    tx: mpsc::UnboundedSender<Vec<u8>>,
) {
    let mut buf = vec![0u8; 8192];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                let _ = tx.send(buf[..n].to_vec());
            }
            Err(_) => break,
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

async fn process_bash_chunk(
    chunk: &[u8],
    total_bytes: &mut usize,
    temp_file_path: &mut Option<PathBuf>,
    temp_file: &mut Option<tokio::fs::File>,
    chunks: &mut VecDeque<Vec<u8>>,
    chunks_bytes: &mut usize,
    max_chunks_bytes: usize,
    on_update: Option<&dyn Fn(ToolUpdate)>,
) -> Result<()> {
    *total_bytes = total_bytes.saturating_add(chunk.len());

    if *total_bytes > DEFAULT_MAX_BYTES && temp_file.is_none() {
        let id = Uuid::new_v4().simple().to_string();
        let path = std::env::temp_dir().join(format!("pi-bash-{id}.log"));
        let mut file = tokio::fs::File::create(&path)
            .await
            .map_err(|e| Error::tool("bash", e.to_string()))?;

        // Write buffered chunks to file first so it contains output from the beginning.
        for existing in chunks.iter() {
            file.write_all(existing)
                .await
                .map_err(|e| Error::tool("bash", e.to_string()))?;
        }

        *temp_file_path = Some(path);
        *temp_file = Some(file);
    }

    if let Some(file) = temp_file.as_mut() {
        file.write_all(chunk)
            .await
            .map_err(|e| Error::tool("bash", e.to_string()))?;
    }

    chunks.push_back(chunk.to_vec());
    *chunks_bytes = chunks_bytes.saturating_add(chunk.len());
    while *chunks_bytes > max_chunks_bytes && chunks.len() > 1 {
        if let Some(front) = chunks.pop_front() {
            *chunks_bytes = chunks_bytes.saturating_sub(front.len());
        }
    }

    if let Some(callback) = on_update {
        let full_text = String::from_utf8_lossy(&concat_chunks(chunks)).to_string();
        let truncation = truncate_tail(&full_text, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);

        let mut details_map = serde_json::Map::new();
        if truncation.truncated {
            details_map.insert("truncation".to_string(), serde_json::to_value(&truncation)?);
        }
        if let Some(path) = temp_file_path.as_ref() {
            details_map.insert(
                "fullOutputPath".to_string(),
                serde_json::Value::String(path.display().to_string()),
            );
        }

        let details = if details_map.is_empty() {
            None
        } else {
            Some(serde_json::Value::Object(details_map))
        };

        callback(ToolUpdate {
            content: vec![ContentBlock::Text(TextContent::new(truncation.content))],
            details,
        });
    }

    Ok(())
}

fn kill_process_tree(pid: Option<u32>) {
    let Some(pid) = pid else { return };
    let root = sysinfo::Pid::from_u32(pid);

    let mut sys = sysinfo::System::new_all();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);

    let mut children_map: HashMap<sysinfo::Pid, Vec<sysinfo::Pid>> = HashMap::new();
    for (p, proc_) in sys.processes() {
        if let Some(parent) = proc_.parent() {
            children_map.entry(parent).or_default().push(*p);
        }
    }

    let mut to_kill = Vec::new();
    collect_process_tree(root, &children_map, &mut to_kill);

    // Kill children first.
    for pid in to_kill.into_iter().rev() {
        if let Some(proc_) = sys.process(pid) {
            let _ = proc_.kill();
        }
    }
}

fn collect_process_tree(
    pid: sysinfo::Pid,
    children_map: &HashMap<sysinfo::Pid, Vec<sysinfo::Pid>>,
    out: &mut Vec<sysinfo::Pid>,
) {
    out.push(pid);
    if let Some(children) = children_map.get(&pid) {
        for child in children {
            collect_process_tree(*child, children_map, out);
        }
    }
}

fn format_grep_path(file_path: &Path, search_path: &Path, is_directory: bool) -> String {
    if is_directory {
        if let Ok(rel) = file_path.strip_prefix(search_path) {
            let rel_str = rel.display().to_string().replace('\\', "/");
            if !rel_str.is_empty() && !rel_str.starts_with("..") {
                return rel_str;
            }
        }
    }
    file_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_string()
}

fn get_file_lines<'a>(path: &Path, cache: &'a mut HashMap<PathBuf, Vec<String>>) -> &'a [String] {
    let lines = cache.entry(path.to_path_buf()).or_insert_with(|| {
        let content = std::fs::read_to_string(path).unwrap_or_default();
        let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
        normalized.split('\n').map(str::to_string).collect()
    });
    lines.as_slice()
}

fn find_fd_binary() -> Option<&'static str> {
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
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate_head() {
        let content = "line1\nline2\nline3\nline4\nline5";
        let result = truncate_head(content, 3, 1000);

        assert_eq!(result.content, "line1\nline2\nline3");
        assert!(result.truncated);
        assert_eq!(result.truncated_by, Some(TruncatedBy::Lines));
        assert_eq!(result.total_lines, 5);
        assert_eq!(result.output_lines, 3);
    }

    #[test]
    fn test_truncate_tail() {
        let content = "line1\nline2\nline3\nline4\nline5";
        let result = truncate_tail(content, 3, 1000);

        assert_eq!(result.content, "line3\nline4\nline5");
        assert!(result.truncated);
        assert_eq!(result.truncated_by, Some(TruncatedBy::Lines));
        assert_eq!(result.total_lines, 5);
        assert_eq!(result.output_lines, 3);
    }

    #[test]
    fn test_truncate_by_bytes() {
        let content = "short\nthis is a longer line\nanother";
        let result = truncate_head(content, 100, 15);

        assert!(result.truncated);
        assert_eq!(result.truncated_by, Some(TruncatedBy::Bytes));
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
    fn test_is_image_file() {
        assert!(is_image_file(Path::new("image.png")));
        assert!(is_image_file(Path::new("photo.JPG")));
        assert!(!is_image_file(Path::new("code.rs")));
        assert!(!is_image_file(Path::new("no_extension")));
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
    fn test_add_line_numbers() {
        let content = "first\nsecond\nthird";
        let result = add_line_numbers(content, 10);
        assert!(result.contains("10\tfirst"));
        assert!(result.contains("11\tsecond"));
        assert!(result.contains("12\tthird"));
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
}
