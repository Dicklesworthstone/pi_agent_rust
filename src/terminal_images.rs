//! Terminal image helpers.
//!
//! The current TUI renders images as **stable text placeholders** like
//! `[image: image/png]`. This keeps output deterministic and test-friendly.
//!
//! Encoding helpers for Kitty/iTerm2 are kept around for future native inline
//! rendering support, but are not used by the TUI today.

use base64::Engine as _;
use std::sync::OnceLock;

// ---------------------------------------------------------------------------
// Protocol detection
// ---------------------------------------------------------------------------

/// The image display protocol supported by the user's terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageProtocol {
    /// Kitty graphics protocol (Kitty, WezTerm, Ghostty, Konsole 22+).
    Kitty,
    /// iTerm2 inline image protocol.
    Iterm2,
    /// No inline image support.
    Unsupported,
}

/// Detect which inline-image protocol the current terminal supports.
///
/// Detection is cached for the lifetime of the process.
pub fn detect_protocol() -> ImageProtocol {
    static CACHED: OnceLock<ImageProtocol> = OnceLock::new();
    *CACHED.get_or_init(detect_protocol_uncached)
}

fn detect_protocol_uncached() -> ImageProtocol {
    // iTerm2 detection (very reliable).
    if let Ok(prog) = std::env::var("TERM_PROGRAM") {
        let lower = prog.to_ascii_lowercase();
        if lower == "iterm.app" || lower == "iterm2" {
            return ImageProtocol::Iterm2;
        }
        // WezTerm supports Kitty.
        if lower == "wezterm" {
            return ImageProtocol::Kitty;
        }
    }

    // Ghostty detection.
    if std::env::var("GHOSTTY_RESOURCES_DIR").is_ok() {
        return ImageProtocol::Kitty;
    }

    // TERM-based heuristics.
    if let Ok(term) = std::env::var("TERM") {
        let lower = term.to_ascii_lowercase();
        if lower.contains("kitty") {
            return ImageProtocol::Kitty;
        }
        if lower.contains("xterm-kitty") {
            return ImageProtocol::Kitty;
        }
    }

    // KITTY_WINDOW_ID is set inside Kitty terminal.
    if std::env::var("KITTY_WINDOW_ID").is_ok() {
        return ImageProtocol::Kitty;
    }

    ImageProtocol::Unsupported
}

// ---------------------------------------------------------------------------
// Kitty graphics protocol
// ---------------------------------------------------------------------------

/// Maximum bytes per Kitty chunk payload.
const KITTY_CHUNK_SIZE: usize = 4096;

/// Encode image data for the Kitty graphics protocol.
///
/// Returns the complete escape sequence string that, when written to stdout,
/// displays the image inline.
///
/// `cols` constrains the display width in terminal columns.
pub fn encode_kitty(image_bytes: &[u8], cols: usize) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(image_bytes);
    let mut out = String::with_capacity(b64.len() + 256);

    let chunks: Vec<&str> = b64
        .as_bytes()
        .chunks(KITTY_CHUNK_SIZE)
        .map(|c| std::str::from_utf8(c).unwrap_or(""))
        .collect();

    for (i, chunk) in chunks.iter().enumerate() {
        let is_first = i == 0;
        let is_last = i == chunks.len() - 1;
        let more = u8::from(!is_last);

        if is_first {
            // First chunk: include action=transmit+display, format=100 (auto-detect).
            // c=<cols> constrains display width.
            write_kitty_chunk(&mut out, &format!("a=T,f=100,c={cols},m={more}"), chunk);
        } else {
            // Continuation chunk.
            write_kitty_chunk(&mut out, &format!("m={more}"), chunk);
        }
    }

    out
}

fn write_kitty_chunk(out: &mut String, control: &str, payload: &str) {
    // Kitty uses APC: ESC _ G <control> ; <payload> ESC \
    out.push_str("\x1b_G");
    out.push_str(control);
    out.push(';');
    out.push_str(payload);
    out.push_str("\x1b\\");
}

// ---------------------------------------------------------------------------
// iTerm2 inline image protocol
// ---------------------------------------------------------------------------

/// Encode image data for the iTerm2 inline image protocol.
///
/// Returns the complete escape sequence string.
///
/// `cols` is used to set `width` in character cells.
pub fn encode_iterm2(image_bytes: &[u8], cols: usize) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(image_bytes);
    let size = image_bytes.len();
    // OSC 1337 ; File=<params> : <base64> BEL
    format!("\x1b]1337;File=size={size};width={cols};inline=1:{b64}\x07")
}

// ---------------------------------------------------------------------------
// Placeholder fallback
// ---------------------------------------------------------------------------

/// Generate a text placeholder for terminals that don't support inline images.
pub fn placeholder(mime_type: &str, width: Option<u32>, height: Option<u32>) -> String {
    match (width, height) {
        (Some(w), Some(h)) => format!("[image: {mime_type}, {w}x{h}]"),
        _ => format!("[image: {mime_type}]"),
    }
}

// ---------------------------------------------------------------------------
// Image dimensions helper
// ---------------------------------------------------------------------------

/// Try to read image dimensions from raw bytes without fully decoding.
///
/// Returns `(width, height)` or `None` if the format is unrecognized.
pub fn image_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    // PNG: width at bytes 16..20, height at 20..24 (big-endian).
    if data.len() >= 24 && data.starts_with(b"\x89PNG\r\n\x1A\n") {
        let w = u32::from_be_bytes([data[16], data[17], data[18], data[19]]);
        let h = u32::from_be_bytes([data[20], data[21], data[22], data[23]]);
        return Some((w, h));
    }

    // JPEG: scan for SOF0/SOF2 markers.
    if data.len() >= 4 && data[0] == 0xFF && data[1] == 0xD8 {
        return jpeg_dimensions(data);
    }

    // GIF: width at 6..8, height at 8..10 (little-endian).
    if data.len() >= 10 && (data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a")) {
        let w = u32::from(u16::from_le_bytes([data[6], data[7]]));
        let h = u32::from(u16::from_le_bytes([data[8], data[9]]));
        return Some((w, h));
    }

    None
}

fn jpeg_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2;
    while i + 1 < data.len() {
        if data[i] != 0xFF {
            return None;
        }
        let marker = data[i + 1];
        i += 2;

        // SOF markers (baseline, progressive, etc.).
        if matches!(marker, 0xC0..=0xC3) {
            if i + 7 > data.len() {
                return None;
            }
            let h = u32::from(u16::from_be_bytes([data[i + 3], data[i + 4]]));
            let w = u32::from(u16::from_be_bytes([data[i + 5], data[i + 6]]));
            return Some((w, h));
        }

        // Skip segment.
        if i + 1 >= data.len() {
            return None;
        }
        let seg_len = u16::from_be_bytes([data[i], data[i + 1]]) as usize;
        i += seg_len;
    }
    None
}

// ---------------------------------------------------------------------------
// High-level render function
// ---------------------------------------------------------------------------

/// Render an image for inline terminal display.
///
/// - `image_b64`: base64-encoded image data (as stored in `ImageContent.data`).
/// - `mime_type`: MIME type string (e.g. `"image/png"`).
/// - `max_cols`: maximum display width in terminal columns.
///
/// Returns the string to write to the terminal.
///
/// Note: today this always returns a plain-text placeholder (see module docs).
pub fn render_inline(image_b64: &str, mime_type: &str, max_cols: usize) -> String {
    let _ = max_cols;

    // Keep TUI output deterministic across terminals by always emitting a plain-text
    // placeholder. (Protocol detection + escape-sequence rendering lives in helpers
    // above for future use.)
    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(image_b64) else {
        return placeholder(mime_type, None, None);
    };

    let dims = image_dimensions(&bytes);
    placeholder(mime_type, dims.map(|(w, _)| w), dims.map(|(_, h)| h))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kitty_single_chunk_small_image() {
        // Small payload that fits in one chunk.
        let data = b"hello";
        let result = encode_kitty(data, 40);
        assert!(result.starts_with("\x1b_G"), "Should start with APC");
        assert!(result.contains("a=T"), "First chunk should have a=T");
        assert!(result.contains("f=100"), "Should auto-detect format");
        assert!(result.contains("c=40"), "Should set column constraint");
        assert!(result.contains("m=0"), "Single chunk should have m=0");
        assert!(result.ends_with("\x1b\\"), "Should end with ST");
    }

    #[test]
    fn kitty_multi_chunk_large_payload() {
        // Create payload larger than KITTY_CHUNK_SIZE.
        let data = vec![0u8; 4096];
        let result = encode_kitty(&data, 80);
        // Base64 of 4096 bytes = ~5462 chars, needs 2 chunks.
        let chunk_count = result.matches("\x1b_G").count();
        assert!(
            chunk_count >= 2,
            "Should have at least 2 chunks, got {chunk_count}"
        );
        // First chunk should have m=1 (more to come).
        assert!(result.contains("m=1"), "First chunk should signal more");
        // Last chunk should have m=0.
        let last_chunk_start = result.rfind("\x1b_G").unwrap();
        let last_chunk = &result[last_chunk_start..];
        assert!(last_chunk.contains("m=0"), "Last chunk should signal done");
    }

    #[test]
    fn iterm2_format() {
        let data = b"test image";
        let result = encode_iterm2(data, 60);
        assert!(
            result.starts_with("\x1b]1337;File="),
            "Should start with OSC 1337"
        );
        assert!(result.contains("inline=1"), "Should be inline");
        assert!(
            result.contains(&format!("size={}", data.len())),
            "Should include file size"
        );
        assert!(result.contains("width=60"), "Should include width");
        assert!(result.ends_with('\x07'), "Should end with BEL");
    }

    #[test]
    fn placeholder_with_dimensions() {
        let result = placeholder("image/png", Some(800), Some(600));
        assert_eq!(result, "[image: image/png, 800x600]");
    }

    #[test]
    fn placeholder_without_dimensions() {
        let result = placeholder("image/jpeg", None, None);
        assert_eq!(result, "[image: image/jpeg]");
    }

    #[test]
    fn png_dimensions() {
        // Minimal valid PNG header with 100x50 dimensions.
        let mut data = vec![0u8; 32];
        data[..8].copy_from_slice(b"\x89PNG\r\n\x1A\n");
        // IHDR chunk: length=13, type=IHDR, width=100, height=50
        data[8..12].copy_from_slice(&13u32.to_be_bytes());
        data[12..16].copy_from_slice(b"IHDR");
        data[16..20].copy_from_slice(&100u32.to_be_bytes());
        data[20..24].copy_from_slice(&50u32.to_be_bytes());

        let dims = image_dimensions(&data);
        assert_eq!(dims, Some((100, 50)));
    }

    #[test]
    fn gif_dimensions() {
        let mut data = vec![0u8; 16];
        data[..6].copy_from_slice(b"GIF89a");
        data[6..8].copy_from_slice(&320u16.to_le_bytes());
        data[8..10].copy_from_slice(&240u16.to_le_bytes());

        let dims = image_dimensions(&data);
        assert_eq!(dims, Some((320, 240)));
    }

    #[test]
    fn unknown_format_returns_none() {
        let data = b"definitely not an image";
        assert_eq!(image_dimensions(data), None);
    }

    #[test]
    fn render_inline_unsupported_with_decodable_image() {
        // Force unsupported by not setting any terminal env vars.
        // In CI/test environments, detect_protocol() typically returns Unsupported.
        // We test the placeholder path directly.
        let result = placeholder("image/png", Some(640), Some(480));
        assert!(result.contains("640x480"));
        assert!(result.contains("image/png"));
    }

    #[test]
    fn detect_protocol_is_deterministic() {
        // Calling detect_protocol twice returns the same value (cached).
        let p1 = detect_protocol();
        let p2 = detect_protocol();
        assert_eq!(p1, p2);
    }
}
