//! Terminal image helpers.
//!
//! Pi renders image blocks inline when the terminal advertises a supported
//! protocol (Kitty or iTerm2). Unsupported terminals fall back to stable text
//! placeholders like `[image: image/png]`.

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
    // Valid PNGs must have the IHDR chunk immediately following the signature.
    if data.len() >= 24 && data.starts_with(b"\x89PNG\r\n\x1A\n") && &data[12..16] == b"IHDR" {
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
    while i < data.len() {
        // Find marker prefix.
        while i < data.len() && data[i] != 0xFF {
            i += 1;
        }
        if i >= data.len() {
            return None;
        }

        // Skip any fill bytes (0xFF) to land on the marker byte.
        while i < data.len() && data[i] == 0xFF {
            i += 1;
        }
        if i >= data.len() {
            return None;
        }

        let marker = data[i];
        i += 1;

        // Standalone markers without length payload.
        if matches!(marker, 0x01 | 0xD0..=0xD7) {
            continue;
        }

        // Start-of-scan or end-of-image reached before SOF.
        if matches!(marker, 0xDA | 0xD9) {
            return None;
        }

        if i + 1 >= data.len() {
            return None;
        }
        let seg_len = usize::from(u16::from_be_bytes([data[i], data[i + 1]]));
        if seg_len < 2 || i.saturating_add(seg_len) > data.len() {
            return None;
        }

        // SOF markers (baseline/progressive and less-common extended variants).
        if is_jpeg_sof_marker(marker) {
            if seg_len < 8 {
                return None;
            }
            let h = u32::from(u16::from_be_bytes([data[i + 3], data[i + 4]]));
            let w = u32::from(u16::from_be_bytes([data[i + 5], data[i + 6]]));
            return Some((w, h));
        }

        i += seg_len;
    }
    None
}

const fn is_jpeg_sof_marker(marker: u8) -> bool {
    matches!(
        marker,
        0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF
    )
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
pub fn render_inline(image_b64: &str, mime_type: &str, max_cols: usize) -> String {
    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(image_b64) else {
        return placeholder(mime_type, None, None);
    };

    render_inline_bytes(&bytes, mime_type, max_cols, detect_protocol())
}

fn render_inline_bytes(
    bytes: &[u8],
    mime_type: &str,
    max_cols: usize,
    protocol: ImageProtocol,
) -> String {
    let dims = image_dimensions(bytes);
    let placeholder = placeholder(mime_type, dims.map(|(w, _)| w), dims.map(|(_, h)| h));

    if bytes.is_empty() {
        return placeholder;
    }

    let cols = max_cols.max(1);
    match protocol {
        ImageProtocol::Kitty => {
            let encoded = encode_kitty(bytes, cols);
            if encoded.is_empty() {
                placeholder
            } else {
                encoded
            }
        }
        ImageProtocol::Iterm2 => encode_iterm2(bytes, cols),
        ImageProtocol::Unsupported => placeholder,
    }
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
    fn jpeg_dimensions() {
        // Minimal SOI + SOF0 segment with width=100, height=50.
        let data = vec![
            0xFF, 0xD8, // SOI
            0xFF, 0xC0, // SOF0 marker
            0x00, 0x11, // segment length
            0x08, // precision
            0x00, 0x32, // height
            0x00, 0x64, // width
            0x03, // component count
            0x01, 0x11, 0x00, // Y
            0x02, 0x11, 0x00, // Cb
            0x03, 0x11, 0x00, // Cr
        ];

        let dims = image_dimensions(&data);
        assert_eq!(dims, Some((100, 50)));
    }

    #[test]
    fn jpeg_dimensions_with_fill_bytes_before_sof() {
        // Valid JPEG marker stream with an APP0 segment and extra 0xFF fill bytes.
        let data = vec![
            0xFF, 0xD8, // SOI
            0xFF, 0xE0, // APP0 marker
            0x00, 0x02, // segment length (length field only)
            0xFF, 0xFF, // fill bytes before next marker
            0xC0, // SOF0 marker byte
            0x00, 0x11, // segment length
            0x08, // precision
            0x00, 0x32, // height
            0x00, 0x64, // width
            0x03, // component count
            0x01, 0x11, 0x00, // Y
            0x02, 0x11, 0x00, // Cb
            0x03, 0x11, 0x00, // Cr
        ];

        assert_eq!(image_dimensions(&data), Some((100, 50)));
    }

    #[test]
    fn jpeg_dimensions_supports_extended_sof_markers() {
        // SOF5 (differential sequential DCT) is uncommon but valid.
        let data = vec![
            0xFF, 0xD8, // SOI
            0xFF, 0xC5, // SOF5 marker
            0x00, 0x11, // segment length
            0x08, // precision
            0x00, 0x2A, // height = 42
            0x00, 0x54, // width = 84
            0x03, // component count
            0x01, 0x11, 0x00, // Y
            0x02, 0x11, 0x00, // Cb
            0x03, 0x11, 0x00, // Cr
        ];

        assert_eq!(image_dimensions(&data), Some((84, 42)));
    }

    #[test]
    fn unknown_format_returns_none() {
        let data = b"definitely not an image";
        assert_eq!(image_dimensions(data), None);
    }

    #[test]
    fn render_inline_returns_placeholder_for_invalid_base64() {
        let result = render_inline("%%%not-base64%%%", "image/png", 80);
        assert_eq!(result, "[image: image/png]");
    }

    #[test]
    fn render_inline_with_unknown_image_bytes_omits_dimensions() {
        let b64 = base64::engine::general_purpose::STANDARD.encode(b"not-an-image");
        let result = render_inline(&b64, "image/webp", 80);
        match detect_protocol() {
            ImageProtocol::Kitty => assert!(result.starts_with("\x1b_G")),
            ImageProtocol::Iterm2 => assert!(result.starts_with("\x1b]1337;File=")),
            ImageProtocol::Unsupported => assert_eq!(result, "[image: image/webp]"),
        }
    }

    #[test]
    fn render_inline_unsupported_with_decodable_image() {
        let result =
            render_inline_bytes(b"pretend-png", "image/png", 80, ImageProtocol::Unsupported);
        assert_eq!(result, "[image: image/png]");
    }

    #[test]
    fn render_inline_unsupported_with_known_dimensions_keeps_placeholder_metadata() {
        let result = placeholder("image/png", Some(640), Some(480));
        assert!(result.contains("640x480"));
        assert!(result.contains("image/png"));
    }

    #[test]
    fn render_inline_kitty_with_decodable_image_uses_escape_sequence() {
        let result = render_inline_bytes(b"hello", "image/png", 40, ImageProtocol::Kitty);
        assert!(result.starts_with("\x1b_G"));
        assert!(result.contains("a=T"));
        assert!(result.ends_with("\x1b\\"));
    }

    #[test]
    fn render_inline_iterm2_with_decodable_image_uses_escape_sequence() {
        let result = render_inline_bytes(b"hello", "image/png", 40, ImageProtocol::Iterm2);
        assert!(result.starts_with("\x1b]1337;File="));
        assert!(result.contains("inline=1"));
        assert!(result.ends_with('\x07'));
    }

    #[test]
    fn render_inline_supported_with_empty_bytes_falls_back_to_placeholder() {
        let kitty = render_inline_bytes(b"", "image/png", 40, ImageProtocol::Kitty);
        assert_eq!(kitty, "[image: image/png]");

        let iterm2 = render_inline_bytes(b"", "image/png", 40, ImageProtocol::Iterm2);
        assert_eq!(iterm2, "[image: image/png]");
    }

    #[test]
    fn detect_protocol_is_deterministic() {
        // Calling detect_protocol twice returns the same value (cached).
        let p1 = detect_protocol();
        let p2 = detect_protocol();
        assert_eq!(p1, p2);
    }

    // ── image_dimensions edge cases ──────────────────────────────────

    #[test]
    fn image_dimensions_empty_data() {
        assert_eq!(image_dimensions(&[]), None);
    }

    #[test]
    fn image_dimensions_truncated_png_header() {
        // PNG signature but not enough data for dimensions
        let data = b"\x89PNG\r\n\x1A\n\x00\x00";
        assert_eq!(image_dimensions(data), None);
    }

    #[test]
    fn image_dimensions_truncated_gif_header() {
        let data = b"GIF89a\x01";
        assert_eq!(image_dimensions(data), None);
    }

    #[test]
    fn image_dimensions_jpeg_truncated_sof() {
        // SOI marker but SOF data cut short
        let data = vec![0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x05];
        assert_eq!(image_dimensions(&data), None);
    }

    #[test]
    fn image_dimensions_jpeg_no_sof_marker() {
        // SOI followed by non-FF byte (invalid)
        let data = vec![0xFF, 0xD8, 0x00, 0x00];
        assert_eq!(image_dimensions(&data), None);
    }

    #[test]
    fn image_dimensions_gif87a() {
        let mut data = vec![0u8; 16];
        data[..6].copy_from_slice(b"GIF87a");
        data[6..8].copy_from_slice(&128u16.to_le_bytes());
        data[8..10].copy_from_slice(&64u16.to_le_bytes());
        assert_eq!(image_dimensions(&data), Some((128, 64)));
    }

    // ── placeholder edge cases ───────────────────────────────────────

    #[test]
    fn placeholder_width_only() {
        let result = placeholder("image/png", Some(100), None);
        assert_eq!(result, "[image: image/png]");
    }

    #[test]
    fn placeholder_height_only() {
        let result = placeholder("image/png", None, Some(200));
        assert_eq!(result, "[image: image/png]");
    }

    // ── kitty with empty data ────────────────────────────────────────

    #[test]
    fn kitty_empty_data_produces_empty_output() {
        let result = encode_kitty(&[], 40);
        // Empty bytes → empty base64 → no chunks → empty output
        assert!(result.is_empty());
    }

    // ── iterm2 with empty data ───────────────────────────────────────

    #[test]
    fn iterm2_empty_data() {
        let result = encode_iterm2(&[], 40);
        assert!(result.contains("size=0"));
        assert!(result.contains("width=40"));
    }

    // ── render_inline with valid PNG ─────────────────────────────────

    #[test]
    fn render_inline_with_valid_png_includes_dimensions() {
        let mut png_data = vec![0u8; 32];
        png_data[..8].copy_from_slice(b"\x89PNG\r\n\x1A\n");
        png_data[8..12].copy_from_slice(&13u32.to_be_bytes());
        png_data[12..16].copy_from_slice(b"IHDR");
        png_data[16..20].copy_from_slice(&200u32.to_be_bytes());
        png_data[20..24].copy_from_slice(&150u32.to_be_bytes());

        let result = render_inline_bytes(&png_data, "image/png", 80, ImageProtocol::Unsupported);
        assert_eq!(result, "[image: image/png, 200x150]");
    }

    // ── ImageProtocol enum ──────────────────────────────────────────

    #[test]
    fn image_protocol_equality() {
        assert_eq!(ImageProtocol::Kitty, ImageProtocol::Kitty);
        assert_ne!(ImageProtocol::Kitty, ImageProtocol::Iterm2);
        assert_ne!(ImageProtocol::Iterm2, ImageProtocol::Unsupported);
    }

    mod proptest_terminal_images {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            /// Kitty encoding always starts with APC and ends with ST for non-empty data.
            #[test]
            fn kitty_bookends(data in proptest::collection::vec(any::<u8>(), 1..512), cols in 1..200usize) {
                let result = encode_kitty(&data, cols);
                assert!(result.starts_with("\x1b_G"), "must start with APC");
                assert!(result.ends_with("\x1b\\"), "must end with ST");
            }

            /// Kitty chunk count grows with payload size.
            #[test]
            fn kitty_chunk_count_lower_bound(data in proptest::collection::vec(any::<u8>(), 1..8192)) {
                let result = encode_kitty(&data, 80);
                let b64_len = (data.len() * 4).div_ceil(3); // ceil(n * 4/3)
                let expected_chunks = b64_len.div_ceil(4096);
                let actual_chunks = result.matches("\x1b_G").count();
                assert!(actual_chunks >= expected_chunks.min(1));
            }

            /// Kitty first chunk always includes `a=T` (transmit+display).
            #[test]
            fn kitty_first_chunk_has_action(data in proptest::collection::vec(any::<u8>(), 1..100)) {
                let result = encode_kitty(&data, 40);
                // First chunk starts at position 0
                let first_st = result.find("\x1b\\").unwrap();
                let first_chunk = &result[..first_st];
                assert!(first_chunk.contains("a=T"));
                assert!(first_chunk.contains("f=100"));
            }

            /// iTerm2 encoding includes size, width, and inline=1.
            #[test]
            fn iterm2_format_invariants(data in proptest::collection::vec(any::<u8>(), 0..512), cols in 1..200usize) {
                let result = encode_iterm2(&data, cols);
                assert!(result.starts_with("\x1b]1337;File="));
                assert!(result.contains(&format!("size={}", data.len())));
                assert!(result.contains(&format!("width={cols}")));
                assert!(result.contains("inline=1"));
                assert!(result.ends_with('\x07'));
            }

            /// Placeholder with both dimensions includes WxH.
            #[test]
            fn placeholder_both_dims(w in 1..10000u32, h in 1..10000u32, mime in "[a-z]+/[a-z]+") {
                let result = placeholder(&mime, Some(w), Some(h));
                assert!(result.contains(&format!("{w}x{h}")));
                assert!(result.contains(&mime));
            }

            /// Placeholder without both dimensions omits WxH pattern.
            #[test]
            fn placeholder_missing_dim(w in 1..10000u32, h in 1..10000u32) {
                let dim_pattern = format!("{w}x{h}");
                let result_no_h = placeholder("image/png", Some(w), None);
                assert!(!result_no_h.contains(&dim_pattern));
                assert_eq!(result_no_h, "[image: image/png]");
                let result_no_w = placeholder("image/png", None, Some(h));
                assert!(!result_no_w.contains(&dim_pattern));
                assert_eq!(result_no_w, "[image: image/png]");
                let result_none = placeholder("image/png", None, None);
                assert_eq!(result_none, "[image: image/png]");
            }

            /// PNG dimension extraction is correct for arbitrary width/height.
            #[test]
            fn png_dimensions_roundtrip(w in 1..10000u32, h in 1..10000u32) {
                let mut data = vec![0u8; 32];
                data[..8].copy_from_slice(b"\x89PNG\r\n\x1A\n");
                data[8..12].copy_from_slice(&13u32.to_be_bytes());
                data[12..16].copy_from_slice(b"IHDR");
                data[16..20].copy_from_slice(&w.to_be_bytes());
                data[20..24].copy_from_slice(&h.to_be_bytes());
                assert_eq!(image_dimensions(&data), Some((w, h)));
            }

            /// GIF dimension extraction is correct for arbitrary width/height.
            #[test]
            fn gif_dimensions_roundtrip(w in 1..65535u16, h in 1..65535u16) {
                let mut data = vec![0u8; 16];
                data[..6].copy_from_slice(b"GIF89a");
                data[6..8].copy_from_slice(&w.to_le_bytes());
                data[8..10].copy_from_slice(&h.to_le_bytes());
                assert_eq!(image_dimensions(&data), Some((u32::from(w), u32::from(h))));
            }

            /// Arbitrary bytes that don't match any magic return None.
            #[test]
            fn unknown_format_none(data in proptest::collection::vec(any::<u8>(), 0..64)) {
                // Skip valid magic bytes
                if data.len() >= 8 && data.starts_with(b"\x89PNG\r\n\x1A\n") {
                    return Ok(());
                }
                if data.len() >= 4 && data.first() == Some(&0xFF) && data.get(1) == Some(&0xD8) {
                    return Ok(());
                }
                if data.len() >= 10 && (data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a")) {
                    return Ok(());
                }
                assert_eq!(image_dimensions(&data), None);
            }

            /// `render_inline` never panics regardless of base64 input.
            #[test]
            fn render_inline_never_panics(b64 in "\\PC{0,100}", mime in "[a-z]+/[a-z]+") {
                let _ = render_inline(&b64, &mime, 80);
            }

            /// Unsupported terminals preserve dimensions in placeholders.
            #[test]
            fn render_inline_png_has_dims(w in 1..5000u32, h in 1..5000u32) {
                let mut png = vec![0u8; 32];
                png[..8].copy_from_slice(b"\x89PNG\r\n\x1A\n");
                png[8..12].copy_from_slice(&13u32.to_be_bytes());
                png[12..16].copy_from_slice(b"IHDR");
                png[16..20].copy_from_slice(&w.to_be_bytes());
                png[20..24].copy_from_slice(&h.to_be_bytes());
                let result = render_inline_bytes(&png, "image/png", 80, ImageProtocol::Unsupported);
                assert!(result.contains(&format!("{w}x{h}")));
            }

            /// Unsupported terminals always include the MIME label in the placeholder.
            #[test]
            fn render_inline_preserves_mime_label(
                data in proptest::collection::vec(any::<u8>(), 0..512),
                mime in "[a-z]{1,10}/[a-z0-9.+-]{1,20}"
            ) {
                let result = render_inline_bytes(&data, &mime, 80, ImageProtocol::Unsupported);
                assert!(result.contains(&mime));
            }

            /// `is_jpeg_sof_marker` accepts exactly the documented SOF range.
            #[test]
            fn sof_marker_classification(marker in 0u8..=255u8) {
                let expected = matches!(
                    marker,
                    0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF
                );
                assert_eq!(is_jpeg_sof_marker(marker), expected);
            }
        }
    }
}
