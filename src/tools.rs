//! Built-in tool implementations.
//!
//! Pi provides 7 built-in tools: read, bash, edit, write, grep, find, ls.

use crate::config::Config;
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
        on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput>;
}

/// Tool execution output.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolOutput {
    pub content: Vec<ContentBlock>,
    pub details: Option<serde_json::Value>,
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
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024; // 50KB

/// Maximum line length for grep results.
pub const GREP_MAX_LINE_LENGTH: usize = 500;

/// Default grep result limit.
pub const DEFAULT_GREP_LIMIT: usize = 100;

/// Default find result limit.
pub const DEFAULT_FIND_LIMIT: usize = 1000;

/// Default ls result limit.
pub const DEFAULT_LS_LIMIT: usize = 500;

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
pub(crate) fn resolve_read_path(file_path: &str, cwd: &Path) -> PathBuf {
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
    use std::path::Component;

    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = out.pop();
            }
            Component::Normal(part) => out.push(part),
        }
    }
    out
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

        let meta = std::fs::metadata(&absolute_path).map_err(|e| {
            Error::tool(
                "read",
                format!("Cannot access file {}: {e}", absolute_path.display()),
            )
        })?;
        if meta.len() == 0 {
            continue;
        }

        let bytes = std::fs::read(&absolute_path).map_err(|e| {
            Error::tool(
                "read",
                format!("Could not read file {}: {e}", absolute_path.display()),
            )
        })?;

        if let Some(mime_type) = detect_supported_image_mime_type_from_bytes(&bytes) {
            let resized = if auto_resize_images {
                resize_image_if_needed(&bytes, mime_type)?
            } else {
                ResizedImage::original(bytes, mime_type)
            };

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
                    let scale = f64::from(ow) / f64::from(w);
                    format!(
                        "[Image: original {ow}x{oh}, displayed at {w}x{h}. Multiply coordinates by {scale:.2} to map to original image.]"
                    )
                } else {
                    String::new()
                }
            } else {
                String::new()
            };

            let path_str = absolute_path.display();
            if note.is_empty() {
                let _ = writeln!(out.text, "<file name=\"{path_str}\"></file>");
            } else {
                let _ = writeln!(out.text, "<file name=\"{path_str}\">{note}</file>");
            }
            continue;
        }

        let content = String::from_utf8_lossy(&bytes);
        let path_str = absolute_path.display();
        let _ = writeln!(out.text, "<file name=\"{path_str}\">");
        out.text.push_str(&content);
        if !content.ends_with('\n') {
            out.text.push('\n');
        }
        let _ = writeln!(out.text, "</file>");
    }

    Ok(out)
}

/// Resolve a file path relative to the current working directory.
/// Public alias for `resolve_to_cwd` used by tools.
fn resolve_path(file_path: &str, cwd: &Path) -> PathBuf {
    resolve_to_cwd(file_path, cwd)
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
    use image::{GenericImageView, ImageEncoder};

    const MAX_WIDTH: u32 = 2000;
    const MAX_HEIGHT: u32 = 2000;
    const MAX_BYTES: usize = 4_718_592; // 4.5MB (below Anthropic's 5MB limit)
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

    let Ok(img) = image::load_from_memory(bytes) else {
        return Ok(ResizedImage::original(bytes.to_vec(), mime_type));
    };

    let (original_width, original_height) = img.dimensions();
    let original_size = bytes.len();

    if original_width <= MAX_WIDTH && original_height <= MAX_HEIGHT && original_size <= MAX_BYTES {
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

    if best.0.len() <= MAX_BYTES {
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
        if best.0.len() <= MAX_BYTES {
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
            if best.0.len() <= MAX_BYTES {
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
pub(crate) fn resize_image_if_needed(
    bytes: &[u8],
    mime_type: &'static str,
) -> Result<ResizedImage> {
    Ok(ResizedImage::original(bytes.to_vec(), mime_type))
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
    offset: Option<i64>,
    limit: Option<i64>,
}

pub struct ReadTool {
    cwd: PathBuf,
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

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &'static str {
        "read"
    }
    fn label(&self) -> &'static str {
        "read"
    }
    fn description(&self) -> &'static str {
        "Read the contents of a file. Supports text files and images (jpg, png, gif, webp). Images are sent as attachments. For text files, output is truncated to 2000 lines or 50KB (whichever is hit first). Use offset/limit for large files. When you need the full file, continue with offset until complete."
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
                    "type": "number",
                    "description": "Line number to start reading from (1-indexed)"
                },
                "limit": {
                    "type": "number",
                    "description": "Maximum number of lines to read"
                }
            },
            "required": ["path"]
        })
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

        let path = resolve_read_path(&input.path, &self.cwd);

        let bytes = tokio::fs::read(&path)
            .await
            .map_err(|e| Error::tool("read", e.to_string()))?;

        if let Some(mime_type) = detect_supported_image_mime_type_from_bytes(&bytes) {
            if self.block_images {
                return Err(Error::tool(
                    "read",
                    "Images are blocked by configuration".to_string(),
                ));
            }

            let resized = if self.auto_resize {
                resize_image_if_needed(&bytes, mime_type)?
            } else {
                ResizedImage::original(bytes.clone(), mime_type)
            };

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
                    let scale = f64::from(ow) / f64::from(w);
                    let _ = write!(
                        note,
                        "\n[Image: original {ow}x{oh}, displayed at {w}x{h}. Multiply coordinates by {scale:.2} to map to original image.]"
                    );
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
            });
        }

        let text_content = String::from_utf8_lossy(&bytes).to_string();

        // Handle empty file specially - return empty content
        if text_content.is_empty() {
            return Ok(ToolOutput {
                content: vec![ContentBlock::Text(TextContent::new(""))],
                details: None,
            });
        }

        let all_lines: Vec<&str> = text_content.split('\n').collect();
        let total_file_lines = all_lines.len();

        let start_line: usize = match input.offset {
            Some(n) if n > 0 => n.saturating_sub(1).try_into().unwrap_or_default(),
            _ => 0,
        };
        let start_line_display = start_line.saturating_add(1);

        if start_line >= all_lines.len() {
            let offset_display = input.offset.unwrap_or(0);
            return Err(Error::tool(
                "read",
                format!(
                    "Offset {offset_display} is beyond end of file ({total_file_lines} lines total)"
                ),
            ));
        }

        // Determine end line based on user limit
        let (end_line, user_limited_lines): (usize, Option<usize>) = input.limit.map_or_else(
            || (all_lines.len(), None),
            |limit| {
                let limit_usize = if limit > 0 {
                    usize::try_from(limit).unwrap_or(0)
                } else {
                    0
                };
                let end = start_line.saturating_add(limit_usize).min(all_lines.len());
                (end, Some(end.saturating_sub(start_line)))
            },
        );

        // Format lines with line numbers (cat -n style)
        // Format: "     N→content" where N is right-aligned
        let max_line_num = end_line;
        let line_num_width = max_line_num.to_string().len().max(5);
        let selected_content: String = all_lines[start_line..end_line]
            .iter()
            .enumerate()
            .map(|(i, line)| {
                let line_num = start_line + i + 1;
                format!("{line_num:>line_num_width$}→{line}")
            })
            .collect::<Vec<_>>()
            .join("\n");

        let truncation = truncate_head(&selected_content, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);

        let mut output_text = truncation.content.clone();
        let mut details: Option<serde_json::Value> = None;

        if truncation.first_line_exceeds_limit {
            let first_line = all_lines.get(start_line).copied().unwrap_or("");
            let first_line_size = format_size(first_line.len());
            output_text = format!(
                "[Line {start_line_display} is {first_line_size}, exceeds {} limit. Use bash: sed -n '{start_line_display}p' {} | head -c {DEFAULT_MAX_BYTES}]",
                format_size(DEFAULT_MAX_BYTES),
                input.path
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
                    "\n\n[Showing lines {start_line_display}-{end_line_display} of {total_file_lines}. Use offset={next_offset} to continue.]"
                );
            } else {
                let _ = write!(
                    output_text,
                    "\n\n[Showing lines {start_line_display}-{end_line_display} of {total_file_lines} ({} limit). Use offset={next_offset} to continue.]",
                    format_size(DEFAULT_MAX_BYTES)
                );
            }

            details = Some(serde_json::json!({ "truncation": truncation }));
        } else if let Some(user_limited) = user_limited_lines {
            if start_line.saturating_add(user_limited) < all_lines.len() {
                let remaining = all_lines
                    .len()
                    .saturating_sub(start_line.saturating_add(user_limited));
                let next_offset = start_line.saturating_add(user_limited).saturating_add(1);
                let _ = write!(
                    output_text,
                    "\n\n[{remaining} more lines in file. Use offset={next_offset} to continue.]"
                );
            }
        }

        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(output_text))],
            details,
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
impl Tool for BashTool {
    fn name(&self) -> &'static str {
        "bash"
    }
    fn label(&self) -> &'static str {
        "bash"
    }
    fn description(&self) -> &'static str {
        "Execute a bash command in the current working directory. Returns stdout and stderr. Output is truncated to last 2000 lines or 50KB (whichever is hit first). If truncated, full output is saved to a temp file. Optionally provide a timeout in seconds."
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
                    "type": "number",
                    "description": "Timeout in seconds (optional, no default timeout)"
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

        let timeout_secs = input.timeout.filter(|t| *t > 0);
        let command = if let Some(prefix) = self
            .command_prefix
            .as_ref()
            .filter(|p| !p.trim().is_empty())
        {
            format!("{prefix}\n{}", input.command)
        } else {
            input.command.clone()
        };

        if !self.cwd.exists() {
            return Err(Error::tool(
                "bash",
                format!(
                    "Working directory does not exist: {}\nCannot execute bash commands.",
                    self.cwd.display()
                ),
            ));
        }

        let shell = self.shell_path.as_deref().unwrap_or_else(|| {
            // Check common bash locations (varies by platform)
            for path in ["/bin/bash", "/usr/bin/bash", "/usr/local/bin/bash"] {
                if Path::new(path).exists() {
                    return path;
                }
            }
            "sh"
        });

        // Spawn the bash process
        let mut child = Command::new(shell)
            .arg("-c")
            .arg(&command)
            .current_dir(&self.cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
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

        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(1024);
        tokio::spawn(pump_stream(stdout, tx.clone()));
        tokio::spawn(pump_stream(stderr, tx));

        let max_chunks_bytes = DEFAULT_MAX_BYTES.saturating_mul(2);
        let mut bash_output = BashOutputState::new(max_chunks_bytes);

        let mut timed_out = false;
        let mut exit_code: Option<i32> = None;
        let child_pid = child.id();

        let timeout_sleep =
            timeout_secs.map(|t| tokio::time::sleep(tokio::time::Duration::from_secs(t)));
        tokio::pin!(timeout_sleep);

        loop {
            tokio::select! {
                maybe_chunk = rx.recv() => {
                    if let Some(chunk) = maybe_chunk {
                        process_bash_chunk(&chunk, &mut bash_output, on_update.as_deref()).await?;
                    } else {
                        break;
                    }
                }
                status = child.wait() => {
                    exit_code = status
                        .map_err(|e| Error::tool("bash", e.to_string()))?
                        .code();
                    break;
                }
                () = async {
                    if let Some(sleep) = timeout_sleep.as_mut().as_pin_mut() {
                        sleep.await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => {
                    timed_out = true;
                    kill_process_tree(child_pid);
                    let _ = child.wait().await;
                    break;
                }
            }
        }

        // Drain any remaining output.
        while let Some(chunk) = rx.recv().await {
            process_bash_chunk(&chunk, &mut bash_output, None).await?;
        }

        // Wait for exit code if we haven't gotten it yet (channel closed before child.wait() was selected)
        if exit_code.is_none() && !timed_out {
            exit_code = child
                .wait()
                .await
                .map_err(|e| Error::tool("bash", e.to_string()))?
                .code();
        }

        drop(bash_output.temp_file.take());

        let full_output = String::from_utf8_lossy(&concat_chunks(&bash_output.chunks)).to_string();

        if timed_out {
            let mut output = full_output;
            if !output.is_empty() {
                output.push_str("\n\n");
            }
            let timeout_display = timeout_secs.unwrap_or(0);
            let _ = write!(output, "Command timed out after {timeout_display} seconds");
            return Err(Error::tool("bash", output));
        }

        let mut truncation = truncate_tail(&full_output, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
        // Correct truncation metadata if we dropped data from the in-memory buffer.
        // bash_output.chunks only holds the last ~100KB, so if total_bytes is larger,
        // we have definitely truncated the head of the output.
        if bash_output.total_bytes > bash_output.chunks_bytes {
            truncation.truncated = true;
            truncation.truncated_by = Some(TruncatedBy::Bytes);
            truncation.total_bytes = bash_output.total_bytes;
        }

        let mut output_text = if truncation.content.is_empty() {
            "(no output)".to_string()
        } else {
            truncation.content.clone()
        };

        let mut details_map = serde_json::Map::new();
        if truncation.truncated {
            details_map.insert("truncation".to_string(), serde_json::to_value(&truncation)?);
            if let Some(path) = bash_output.temp_file_path.as_ref() {
                details_map.insert(
                    "fullOutputPath".to_string(),
                    serde_json::Value::String(path.display().to_string()),
                );
            }

            let start_line = truncation
                .total_lines
                .saturating_sub(truncation.output_lines)
                .saturating_add(1);
            let end_line = truncation.total_lines;

            let full_output_path = bash_output
                .temp_file_path
                .as_ref()
                .map_or_else(|| "undefined".to_string(), |p| p.display().to_string());

            if truncation.last_line_partial {
                let last_line = full_output.split('\n').next_back().unwrap_or("");
                let last_line_size = format_size(last_line.len());
                let _ = write!(
                    output_text,
                    "\n\n[Showing last {} of line {end_line} (line is {last_line_size}). Full output: {full_output_path}]",
                    format_size(truncation.output_bytes)
                );
            } else if truncation.truncated_by == Some(TruncatedBy::Lines) {
                let _ = write!(
                    output_text,
                    "\n\n[Showing lines {start_line}-{end_line} of {}. Full output: {full_output_path}]",
                    truncation.total_lines
                );
            } else {
                let _ = write!(
                    output_text,
                    "\n\n[Showing lines {start_line}-{end_line} of {} ({} limit). Full output: {full_output_path}]",
                    truncation.total_lines,
                    format_size(DEFAULT_MAX_BYTES)
                );
            }
        }

        if let Some(code) = exit_code {
            if code != 0 {
                let _ = write!(output_text, "\n\nCommand exited with code {code}");
                return Err(Error::tool("bash", output_text));
            }
        }

        let details = if details_map.is_empty() {
            None
        } else {
            Some(serde_json::Value::Object(details_map))
        };

        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(output_text))],
            details,
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

fn strip_bom(s: &str) -> (String, bool) {
    s.strip_prefix('\u{FEFF}').map_or_else(
        || (s.to_string(), false),
        |stripped| (stripped.to_string(), true),
    )
}

fn detect_line_ending(content: &str) -> &'static str {
    let crlf_idx = content.find("\r\n");
    let lf_idx = content.find('\n');
    if lf_idx.is_none() {
        return "\n";
    }
    let Some(crlf_idx) = crlf_idx else {
        return "\n";
    };
    let lf_idx = lf_idx.unwrap_or(usize::MAX);
    if crlf_idx < lf_idx { "\r\n" } else { "\n" }
}

fn normalize_to_lf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

fn restore_line_endings(text: &str, ending: &str) -> String {
    if ending == "\r\n" {
        text.replace('\n', "\r\n")
    } else {
        text.to_string()
    }
}

fn normalize_for_fuzzy_match_text(text: &str) -> String {
    let trimmed = text
        .split('\n')
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n");
    let s = normalize_unicode_spaces(&trimmed);
    let s = normalize_quotes(&s);
    normalize_dashes(&s)
}

#[derive(Debug, Clone)]
struct FuzzyMatchResult {
    found: bool,
    index: usize,
    match_length: usize,
    content_for_replacement: String,
}

/// Build a mapping from normalized byte indices to original byte indices.
/// Returns (normalized_string, Vec<original_byte_index_for_each_normalized_byte>).
fn build_normalized_with_mapping(content: &str) -> (String, Vec<usize>) {
    let mut normalized = String::with_capacity(content.len());
    let mut mapping = Vec::with_capacity(content.len());

    // Process line by line to handle trailing whitespace normalization
    let lines: Vec<&str> = content.split('\n').collect();
    let last_line_idx = lines.len().saturating_sub(1);

    let mut original_byte_offset = 0;

    for (line_idx, line) in lines.iter().enumerate() {
        // Trim trailing whitespace for fuzzy matching
        let trimmed_len = line.trim_end().len();

        for (char_offset, c) in line.char_indices() {
            // Skip trailing whitespace (chars beyond trimmed_len)
            if char_offset >= trimmed_len {
                continue;
            }

            let orig_byte_idx = original_byte_offset + char_offset;

            // Normalize the character
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

            let char_len_in_normalized = normalized_char.len_utf8();
            normalized.push(normalized_char);
            for _ in 0..char_len_in_normalized {
                mapping.push(orig_byte_idx);
            }
        }

        // Move past this line in original content
        original_byte_offset += line.len();

        // Add newline if not the last line
        if line_idx < last_line_idx {
            normalized.push('\n');
            mapping.push(original_byte_offset); // Points to the \n in original
            original_byte_offset += 1; // Move past the \n
        }
    }

    (normalized, mapping)
}

fn fuzzy_find_text(content: &str, old_text: &str) -> FuzzyMatchResult {
    // First, try exact match (fastest path)
    if let Some(index) = content.find(old_text) {
        return FuzzyMatchResult {
            found: true,
            index,
            match_length: old_text.len(),
            content_for_replacement: content.to_string(),
        };
    }

    // Build normalized versions with index mapping
    let (normalized_content, content_mapping) = build_normalized_with_mapping(content);
    let (normalized_old_text, _) = build_normalized_with_mapping(old_text);

    // Try to find the normalized old_text in normalized content
    if let Some(normalized_index) = normalized_content.find(&normalized_old_text) {
        // Map the normalized index back to original content
        let original_start = if normalized_index < content_mapping.len() {
            content_mapping[normalized_index]
        } else {
            // Edge case: match at very end
            content.len()
        };

        // Find the end position in original content
        let normalized_end = normalized_index + normalized_old_text.len();
        let original_end = if normalized_end < content_mapping.len() {
            // Get the byte position after the match
            // We need to find where the next character starts in original
            content_mapping[normalized_end]
        } else {
            // Match goes to end of content
            content.len()
        };

        // But we need the actual end including any trailing whitespace that was stripped
        // Find the end of the matched region in original content by looking at line structure
        let original_match_len = original_end - original_start;

        return FuzzyMatchResult {
            found: true,
            index: original_start,
            match_length: original_match_len,
            content_for_replacement: content.to_string(),
        };
    }

    FuzzyMatchResult {
        found: false,
        index: 0,
        match_length: 0,
        content_for_replacement: content.to_string(),
    }
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
    let mut current_value = String::new();

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
            if !current_value.is_empty() {
                current_value.push('\n');
            }
            current_value.push_str(line);
        } else {
            if let Some(prev_tag) = current_tag {
                parts.push(DiffPart {
                    tag: prev_tag,
                    value: current_value,
                });
            }
            current_tag = Some(tag);
            current_value = line.to_string();
        }
    }

    if let Some(tag) = current_tag {
        parts.push(DiffPart {
            tag,
            value: current_value,
        });
    }

    parts
}

fn generate_diff_string(old_content: &str, new_content: &str) -> (String, Option<usize>) {
    let parts = diff_parts(old_content, new_content);
    let mut output: Vec<String> = Vec::new();

    let old_line_count = old_content.split('\n').count();
    let new_line_count = new_content.split('\n').count();
    let max_line_num = old_line_count.max(new_line_count).max(1);
    let line_num_width = max_line_num.to_string().len();

    let mut old_line_num: usize = 1;
    let mut new_line_num: usize = 1;
    let mut last_was_change = false;
    let mut first_changed_line: Option<usize> = None;
    let context_lines: usize = 4;

    for (i, part) in parts.iter().enumerate() {
        let mut raw: Vec<&str> = part.value.split('\n').collect();
        if raw.last().is_some_and(|l| l.is_empty()) {
            raw.pop();
        }

        match part.tag {
            DiffTag::Added | DiffTag::Removed => {
                if first_changed_line.is_none() {
                    first_changed_line = Some(new_line_num);
                }

                for line in raw {
                    match part.tag {
                        DiffTag::Added => {
                            let line_num = format!("{new_line_num:>line_num_width$}");
                            output.push(format!("+{line_num} {line}"));
                            new_line_num = new_line_num.saturating_add(1);
                        }
                        DiffTag::Removed => {
                            let line_num = format!("{old_line_num:>line_num_width$}");
                            output.push(format!("-{line_num} {line}"));
                            old_line_num = old_line_num.saturating_add(1);
                        }
                        DiffTag::Equal => {}
                    }
                }

                last_was_change = true;
            }
            DiffTag::Equal => {
                let next_part_is_change = i < parts.len().saturating_sub(1)
                    && matches!(parts[i + 1].tag, DiffTag::Added | DiffTag::Removed);

                if last_was_change || next_part_is_change {
                    let mut lines_to_show: Vec<&str> = raw.clone();
                    let mut skip_start: usize = 0;
                    let mut skip_end: usize = 0;

                    if !last_was_change {
                        skip_start = raw.len().saturating_sub(context_lines);
                        lines_to_show = raw[skip_start..].to_vec();
                    }

                    if !next_part_is_change && lines_to_show.len() > context_lines {
                        skip_end = lines_to_show.len().saturating_sub(context_lines);
                        lines_to_show = lines_to_show[..context_lines].to_vec();
                    }

                    if skip_start > 0 {
                        output.push(format!(" {} ...", " ".repeat(line_num_width)));
                        old_line_num = old_line_num.saturating_add(skip_start);
                        new_line_num = new_line_num.saturating_add(skip_start);
                    }

                    for line in lines_to_show {
                        let line_num = format!("{old_line_num:>line_num_width$}");
                        output.push(format!(" {line_num} {line}"));
                        old_line_num = old_line_num.saturating_add(1);
                        new_line_num = new_line_num.saturating_add(1);
                    }

                    if skip_end > 0 {
                        output.push(format!(" {} ...", " ".repeat(line_num_width)));
                        old_line_num = old_line_num.saturating_add(skip_end);
                        new_line_num = new_line_num.saturating_add(skip_end);
                    }
                } else {
                    old_line_num = old_line_num.saturating_add(raw.len());
                    new_line_num = new_line_num.saturating_add(raw.len());
                }

                last_was_change = false;
            }
        }
    }

    (output.join("\n"), first_changed_line)
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &'static str {
        "edit"
    }
    fn label(&self) -> &'static str {
        "edit"
    }
    fn description(&self) -> &'static str {
        "Edit a file by replacing exact text. The oldText must match exactly (including whitespace). Use this for precise, surgical edits."
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
                    "description": "Exact text to find and replace (must match exactly)"
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

        let absolute_path = resolve_path(&input.path, &self.cwd);

        // Match legacy behavior: any access failure is reported as "File not found".
        if tokio::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&absolute_path)
            .await
            .is_err()
        {
            return Err(Error::tool(
                "edit",
                format!("File not found: {}", input.path),
            ));
        }

        // Read bytes and decode lossily as UTF-8 (Node Buffer.toString("utf-8") behavior).
        let raw = tokio::fs::read(&absolute_path)
            .await
            .map_err(|e| Error::tool("edit", format!("Failed to read file: {e}")))?;
        let raw_content = String::from_utf8_lossy(&raw).to_string();

        // Strip BOM before matching (LLM won't include invisible BOM in oldText).
        let (content_no_bom, had_bom) = strip_bom(&raw_content);

        let original_ending = detect_line_ending(&content_no_bom);
        let normalized_content = normalize_to_lf(&content_no_bom);
        let normalized_old_text = normalize_to_lf(&input.old_text);
        let normalized_new_text = normalize_to_lf(&input.new_text);

        let match_result = fuzzy_find_text(&normalized_content, &normalized_old_text);
        if !match_result.found {
            return Err(Error::tool(
                "edit",
                format!(
                    "Could not find the exact text in {}. The old text must match exactly including all whitespace and newlines.",
                    input.path
                ),
            ));
        }

        // Count occurrences using fuzzy-normalized content (legacy behavior).
        let fuzzy_content = normalize_for_fuzzy_match_text(&normalized_content);
        let fuzzy_old_text = normalize_for_fuzzy_match_text(&normalized_old_text);
        let occurrences = if fuzzy_old_text.is_empty() {
            0
        } else {
            fuzzy_content
                .split(&fuzzy_old_text)
                .count()
                .saturating_sub(1)
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

        // Perform replacement in the matched coordinate space (exact or fuzzy-normalized).
        let base_content = match_result.content_for_replacement;
        let idx = match_result.index;
        let match_len = match_result.match_length;

        let mut new_content = String::new();
        new_content.push_str(&base_content[..idx]);
        new_content.push_str(&normalized_new_text);
        new_content.push_str(&base_content[idx + match_len..]);

        if base_content == new_content {
            return Err(Error::tool(
                "edit",
                format!(
                    "No changes made to {}. The replacement produced identical content. This might indicate an issue with special characters or the text not existing as expected.",
                    input.path
                ),
            ));
        }

        // Restore original line endings and re-add BOM if present.
        let mut final_content = restore_line_endings(&new_content, original_ending);
        if had_bom {
            final_content = format!("\u{FEFF}{final_content}");
        }

        // Atomic write (safe improvement vs legacy, behavior-equivalent).
        let parent = absolute_path.parent().unwrap_or_else(|| Path::new("."));
        let temp_file = tempfile::NamedTempFile::new_in(parent)
            .map_err(|e| Error::tool("edit", format!("Failed to create temp file: {e}")))?;
        tokio::fs::write(temp_file.path(), &final_content)
            .await
            .map_err(|e| Error::tool("edit", format!("Failed to write temp file: {e}")))?;
        temp_file
            .persist(&absolute_path)
            .map_err(|e| Error::tool("edit", format!("Failed to persist file: {e}")))?;

        let (diff, first_changed_line) = generate_diff_string(&base_content, &new_content);
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
        "write"
    }
    fn description(&self) -> &'static str {
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

        let path = resolve_path(&input.path, &self.cwd);

        // Create parent directories if needed
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| Error::tool("write", format!("Failed to create directories: {e}")))?;
        }

        let bytes_written = js_string_length(&input.content);

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
                bytes_written, input.path
            )))],
            details: None,
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
        "grep"
    }
    fn description(&self) -> &'static str {
        "Search file contents for a pattern. Returns matching lines with file paths and line numbers. Respects .gitignore. Output is truncated to 100 matches or 50KB (whichever is hit first). Long lines are truncated to 500 chars."
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
                    "type": "number",
                    "description": "Number of lines to show before and after each match (default: 0)"
                },
                "limit": {
                    "type": "number",
                    "description": "Maximum number of matches to return (default: 100)"
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
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let input: GrepInput =
            serde_json::from_value(input).map_err(|e| Error::validation(e.to_string()))?;

        if !rg_available() {
            return Err(Error::tool(
                "grep",
                "ripgrep (rg) is not available and could not be downloaded".to_string(),
            ));
        }

        let search_dir = input.path.as_deref().unwrap_or(".");
        let search_path = resolve_path(search_dir, &self.cwd);

        let is_directory = std::fs::metadata(&search_path)
            .map_err(|e| {
                Error::tool(
                    "grep",
                    format!("Cannot access path {}: {e}", search_path.display()),
                )
            })?
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
            .map_err(|e| Error::tool("grep", format!("Failed to run ripgrep: {e}")))?;

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
                let line_text = lines.get(current - 1).map_or("", String::as_str);
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
        "find"
    }
    fn description(&self) -> &'static str {
        "Search for files by glob pattern. Returns matching file paths relative to the search directory. Respects .gitignore. Output is truncated to 1000 results or 50KB (whichever is hit first)."
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
                    "type": "number",
                    "description": "Maximum number of results (default: 1000)"
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
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let input: FindInput =
            serde_json::from_value(input).map_err(|e| Error::validation(e.to_string()))?;

        let search_dir = input.path.as_deref().unwrap_or(".");
        let search_path = resolve_path(search_dir, &self.cwd);
        let effective_limit = input.limit.unwrap_or(DEFAULT_FIND_LIMIT);

        if !search_path.exists() {
            return Err(Error::tool(
                "find",
                format!("Path not found: {}", search_path.display()),
            ));
        }

        let fd_cmd = find_fd_binary().ok_or_else(|| {
            Error::tool(
                "find",
                "fd is not available and could not be downloaded".to_string(),
            )
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

            let mut rel = if Path::new(line).is_absolute() && line.starts_with(&search_path_str) {
                line[search_path_str.len()..]
                    .trim_start_matches(['/', '\\'])
                    .to_string()
            } else {
                line.to_string()
            };

            let full_path = if Path::new(line).is_absolute() {
                PathBuf::from(line)
            } else {
                search_path.join(line)
            };
            if full_path.is_dir() && !rel.ends_with('/') {
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
        "ls"
    }
    fn description(&self) -> &'static str {
        "List directory contents. Returns entries sorted alphabetically, with '/' suffix for directories. Includes dotfiles. Output is truncated to 500 entries or 50KB (whichever is hit first)."
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
                    "type": "number",
                    "description": "Maximum number of entries to return (default: 500)"
                }
            }
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let input: LsInput =
            serde_json::from_value(input).map_err(|e| Error::validation(e.to_string()))?;

        let dir_path = input
            .path
            .as_ref()
            .map_or_else(|| self.cwd.clone(), |p| resolve_path(p, &self.cwd));

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
        let mut read_dir = tokio::fs::read_dir(&dir_path)
            .await
            .map_err(|e| Error::tool("ls", format!("Cannot read directory: {e}")))?;

        while let Some(entry) = read_dir
            .next_entry()
            .await
            .map_err(|e| Error::tool("ls", format!("Cannot read directory: {e}")))?
        {
            let name = entry.file_name().to_string_lossy().to_string();
            let Ok(meta) = entry.metadata().await else {
                continue;
            };
            entries.push((name, meta.is_dir()));
        }

        // Sort alphabetically (case-insensitive).
        entries.sort_by_key(|(a, _)| a.to_lowercase());

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
        })
    }
}

// ============================================================================
// Helper functions
// ============================================================================

fn rg_available() -> bool {
    std::process::Command::new("rg")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

async fn pump_stream<R: tokio::io::AsyncRead + Unpin + Send + 'static>(
    mut reader: R,
    tx: mpsc::Sender<Vec<u8>>,
) {
    let mut buf = vec![0u8; 8192];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if tx.send(buf[..n].to_vec()).await.is_err() {
                    break;
                }
            }
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
    temp_file_path: Option<PathBuf>,
    temp_file: Option<tokio::fs::File>,
    chunks: VecDeque<Vec<u8>>,
    chunks_bytes: usize,
    max_chunks_bytes: usize,
}

impl BashOutputState {
    const fn new(max_chunks_bytes: usize) -> Self {
        Self {
            total_bytes: 0,
            temp_file_path: None,
            temp_file: None,
            chunks: VecDeque::new(),
            chunks_bytes: 0,
            max_chunks_bytes,
        }
    }
}

async fn process_bash_chunk(
    chunk: &[u8],
    state: &mut BashOutputState,
    on_update: Option<&(dyn Fn(ToolUpdate) + Send + Sync)>,
) -> Result<()> {
    state.total_bytes = state.total_bytes.saturating_add(chunk.len());

    if state.total_bytes > DEFAULT_MAX_BYTES && state.temp_file.is_none() {
        let id_full = Uuid::new_v4().simple().to_string();
        let id = &id_full[..16];
        let path = std::env::temp_dir().join(format!("pi-bash-{id}.log"));
        let mut file = tokio::fs::File::create(&path)
            .await
            .map_err(|e| Error::tool("bash", e.to_string()))?;

        // Write buffered chunks to file first so it contains output from the beginning.
        for existing in &state.chunks {
            file.write_all(existing)
                .await
                .map_err(|e| Error::tool("bash", e.to_string()))?;
        }

        state.temp_file_path = Some(path);
        state.temp_file = Some(file);
    }

    if let Some(file) = state.temp_file.as_mut() {
        file.write_all(chunk)
            .await
            .map_err(|e| Error::tool("bash", e.to_string()))?;
    }

    state.chunks.push_back(chunk.to_vec());
    state.chunks_bytes = state.chunks_bytes.saturating_add(chunk.len());
    while state.chunks_bytes > state.max_chunks_bytes && state.chunks.len() > 1 {
        if let Some(front) = state.chunks.pop_front() {
            state.chunks_bytes = state.chunks_bytes.saturating_sub(front.len());
        }
    }

    if let Some(callback) = on_update {
        let full_text = String::from_utf8_lossy(&concat_chunks(&state.chunks)).to_string();
        let truncation = truncate_tail(&full_text, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);

        let mut details_map = serde_json::Map::new();
        if truncation.truncated {
            details_map.insert("truncation".to_string(), serde_json::to_value(&truncation)?);
        }
        if let Some(path) = state.temp_file_path.as_ref() {
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

    let mut sys = sysinfo::System::new();
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
        // Match Node's `readFileSync(..., "utf-8")` behavior: decode lossily rather than failing.
        let bytes = std::fs::read(path).unwrap_or_default();
        let content = String::from_utf8_lossy(&bytes).to_string();
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
}
