//! Snapcompact compaction mode (bd-cv653.7.6).
//!
//! Experimental context-compression mode: alongside the text summary, the
//! compacted conversation span is rasterized into deterministic PNG frames
//! ("terminal-style" bitmap rendering) and attached to the compaction
//! summary message as image blocks. Vision-capable models can then consume
//! the rasterized history; text-only models never see the frames (they are
//! stripped at context-build time with a logged reason).
//!
//! Design invariants:
//! - Determinism: identical transcript bytes produce identical PNG bytes.
//!   Everything here is integer-only rendering plus `flate2` DEFLATE
//!   (miniz_oxide backend is deterministic for a fixed level and input) and
//!   a hand-rolled CRC-32 — no timestamps, no OS entropy, no floats.
//! - No new dependencies: the PNG encoder uses `flate2` (already a crate
//!   dependency) and base64 encoding uses `base64` (already a dependency).
//! - Font: embedded 5×7 fixed-width ASCII font (classic public-domain
//!   `glcdfont` table, chars 0x20–0x7E). Non-printable and non-ASCII input
//!   renders as `'?'`; tabs expand to 4 spaces. Output is ASCII-stable.
//! - Frames are stored inside `CompactionEntry.details` under the
//!   [`SNAPCOMPACT_DETAILS_KEY`] key with a versioned schema, so session
//!   JSONL/SQLite persistence needs zero format changes.

use crate::model::{ContentBlock, ImageContent, Message, TextContent, UserContent, UserMessage};
use base64::Engine as _;
use serde::{Deserialize, Serialize};

/// Schema tag stored with the frames inside `CompactionEntry.details`.
pub const SNAPCOMPACT_DETAILS_SCHEMA: &str = "pi.compaction.snapcompact.v1";

/// Key under `CompactionEntry.details` holding the snapcompact payload.
pub const SNAPCOMPACT_DETAILS_KEY: &str = "snapcompact";

/// Prefix used on compaction summary messages (mirrors
/// `session.rs::COMPACTION_SUMMARY_PREFIX`); used to identify which user
/// messages carry snapcompact frames so stripping only ever touches our own
/// image blocks, never user-pasted images.
pub(crate) const COMPACTION_SUMMARY_PREFIX: &str =
    "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";

// ── Geometry ────────────────────────────────────────────────────────────────

/// Glyph pixel size of the embedded 5×7 font.
const GLYPH_W: u32 = 5;
const GLYPH_H: u32 = 7;
/// Integer upscale applied to every glyph for model readability.
const SCALE: u32 = 2;
/// Horizontal pixels per character cell (glyph + 1px gap, scaled).
const CHAR_ADVANCE: u32 = (GLYPH_W + 1) * SCALE;
/// Vertical pixels per text line (glyph + 1px gap, scaled).
const LINE_HEIGHT: u32 = (GLYPH_H + 1) * SCALE;
/// Frame padding, all sides.
const PAD: u32 = 8;
/// Fixed frame width in pixels.
const FRAME_WIDTH: u32 = 960;
/// Maximum frame height in pixels; taller output splits across frames.
const MAX_FRAME_HEIGHT: u32 = 1280;

/// Characters per rendered line given the fixed frame geometry.
const CHARS_PER_LINE: u32 = (FRAME_WIDTH - 2 * PAD) / CHAR_ADVANCE;
/// Text lines per frame at maximum height.
const LINES_PER_FRAME: u32 = (MAX_FRAME_HEIGHT - 2 * PAD) / LINE_HEIGHT;

// ── Font ────────────────────────────────────────────────────────────────────

/// Classic fixed-width 5×7 ASCII font (public-domain `glcdfont` table),
/// chars 0x20–0x7E. Each glyph is 5 column-bytes; bit 0 of each byte is the
/// top row. Verified against anchors: 'A' = `[0x7C, 0x12, 0x11, 0x12, 0x7C]`,
/// '0' = `[0x3E, 0x51, 0x49, 0x45, 0x3E]`.
#[rustfmt::skip]
const FONT_5X7: [[u8; GLYPH_W as usize]; 95] = [
    [0x00, 0x00, 0x00, 0x00, 0x00], [0x00, 0x00, 0x5F, 0x00, 0x00], [0x00, 0x07, 0x00, 0x07, 0x00], [0x14, 0x7F, 0x14, 0x7F, 0x14], [0x24, 0x2A, 0x7F, 0x2A, 0x12],
    [0x23, 0x13, 0x08, 0x64, 0x62], [0x36, 0x49, 0x56, 0x20, 0x50], [0x00, 0x08, 0x07, 0x03, 0x00], [0x00, 0x1C, 0x22, 0x41, 0x00], [0x00, 0x41, 0x22, 0x1C, 0x00],
    [0x2A, 0x1C, 0x7F, 0x1C, 0x2A], [0x08, 0x08, 0x3E, 0x08, 0x08], [0x00, 0x80, 0x70, 0x30, 0x00], [0x08, 0x08, 0x08, 0x08, 0x08], [0x00, 0x00, 0x60, 0x60, 0x00],
    [0x20, 0x10, 0x08, 0x04, 0x02], [0x3E, 0x51, 0x49, 0x45, 0x3E], [0x00, 0x42, 0x7F, 0x40, 0x00], [0x72, 0x49, 0x49, 0x49, 0x46], [0x21, 0x41, 0x49, 0x4D, 0x33],
    [0x18, 0x14, 0x12, 0x7F, 0x10], [0x27, 0x45, 0x45, 0x45, 0x39], [0x3C, 0x4A, 0x49, 0x49, 0x31], [0x41, 0x21, 0x11, 0x09, 0x07], [0x36, 0x49, 0x49, 0x49, 0x36],
    [0x46, 0x49, 0x49, 0x29, 0x1E], [0x00, 0x00, 0x14, 0x00, 0x00], [0x00, 0x40, 0x34, 0x00, 0x00], [0x00, 0x08, 0x14, 0x22, 0x41], [0x14, 0x14, 0x14, 0x14, 0x14],
    [0x00, 0x41, 0x22, 0x14, 0x08], [0x02, 0x01, 0x59, 0x09, 0x06], [0x3E, 0x41, 0x5D, 0x59, 0x4E], [0x7C, 0x12, 0x11, 0x12, 0x7C], [0x7F, 0x49, 0x49, 0x49, 0x36],
    [0x3E, 0x41, 0x41, 0x41, 0x22], [0x7F, 0x41, 0x41, 0x41, 0x3E], [0x7F, 0x49, 0x49, 0x49, 0x41], [0x7F, 0x09, 0x09, 0x09, 0x01], [0x3E, 0x41, 0x41, 0x51, 0x73],
    [0x7F, 0x08, 0x08, 0x08, 0x7F], [0x00, 0x41, 0x7F, 0x41, 0x00], [0x20, 0x40, 0x41, 0x3F, 0x01], [0x7F, 0x08, 0x14, 0x22, 0x41], [0x7F, 0x40, 0x40, 0x40, 0x40],
    [0x7F, 0x02, 0x1C, 0x02, 0x7F], [0x7F, 0x04, 0x08, 0x10, 0x7F], [0x3E, 0x41, 0x41, 0x41, 0x3E], [0x7F, 0x09, 0x09, 0x09, 0x06], [0x3E, 0x41, 0x51, 0x21, 0x5E],
    [0x7F, 0x09, 0x19, 0x29, 0x46], [0x26, 0x49, 0x49, 0x49, 0x32], [0x03, 0x01, 0x7F, 0x01, 0x03], [0x3F, 0x40, 0x40, 0x40, 0x3F], [0x1F, 0x20, 0x40, 0x20, 0x1F],
    [0x3F, 0x40, 0x38, 0x40, 0x3F], [0x63, 0x14, 0x08, 0x14, 0x63], [0x03, 0x04, 0x78, 0x04, 0x03], [0x61, 0x59, 0x49, 0x4D, 0x43], [0x00, 0x7F, 0x41, 0x41, 0x41],
    [0x02, 0x04, 0x08, 0x10, 0x20], [0x00, 0x41, 0x41, 0x41, 0x7F], [0x04, 0x02, 0x01, 0x02, 0x04], [0x40, 0x40, 0x40, 0x40, 0x40], [0x00, 0x03, 0x07, 0x08, 0x00],
    [0x20, 0x54, 0x54, 0x78, 0x40], [0x7F, 0x28, 0x44, 0x44, 0x38], [0x38, 0x44, 0x44, 0x44, 0x28], [0x38, 0x44, 0x44, 0x28, 0x7F], [0x38, 0x54, 0x54, 0x54, 0x18],
    [0x00, 0x08, 0x7E, 0x09, 0x02], [0x18, 0xA4, 0xA4, 0x9C, 0x78], [0x7F, 0x08, 0x04, 0x04, 0x78], [0x00, 0x44, 0x7D, 0x40, 0x00], [0x20, 0x40, 0x40, 0x3D, 0x00],
    [0x7F, 0x10, 0x28, 0x44, 0x00], [0x00, 0x41, 0x7F, 0x40, 0x00], [0x7C, 0x04, 0x78, 0x04, 0x78], [0x7C, 0x08, 0x04, 0x04, 0x78], [0x38, 0x44, 0x44, 0x44, 0x38],
    [0xFC, 0x18, 0x24, 0x24, 0x18], [0x18, 0x24, 0x24, 0x18, 0xFC], [0x7C, 0x08, 0x04, 0x04, 0x08], [0x48, 0x54, 0x54, 0x54, 0x24], [0x04, 0x04, 0x3F, 0x44, 0x24],
    [0x3C, 0x40, 0x40, 0x20, 0x7C], [0x1C, 0x20, 0x40, 0x20, 0x1C], [0x3C, 0x40, 0x30, 0x40, 0x3C], [0x44, 0x28, 0x10, 0x28, 0x44], [0x4C, 0x90, 0x90, 0x90, 0x7C],
    [0x44, 0x64, 0x54, 0x4C, 0x44], [0x00, 0x08, 0x36, 0x41, 0x00], [0x00, 0x00, 0x77, 0x00, 0x00], [0x00, 0x41, 0x36, 0x08, 0x00], [0x02, 0x01, 0x02, 0x04, 0x02],
];

// ── Palette ─────────────────────────────────────────────────────────────────

type Rgb = [u8; 3];

/// Deterministic role palette ("syntax-aware colors"): lines are colored by
/// their transcript-role prefix (`[User]:`, `[Assistant]:`, tool markers,
/// markdown headers), matching `serialize_conversation`'s labels.
fn color_for_line(line: &str) -> Rgb {
    const BG_DIM_HEADER: Rgb = [240, 240, 245];
    const USER_AMBER: Rgb = [255, 179, 71];
    const ASSISTANT_SKY: Rgb = [96, 181, 255];
    const TOOL_GREEN: Rgb = [134, 222, 116];
    const DEFAULT_TEXT: Rgb = [208, 208, 208];

    if line.starts_with("[User]:") {
        USER_AMBER
    } else if line.starts_with("[Assistant]:") {
        ASSISTANT_SKY
    } else if line.starts_with("[Tool Call]") || line.starts_with("[Tool Result]") {
        TOOL_GREEN
    } else if line.starts_with('#') {
        BG_DIM_HEADER
    } else {
        DEFAULT_TEXT
    }
}

const BACKGROUND: Rgb = [16, 16, 20];

// ── Payload types ───────────────────────────────────────────────────────────

/// One rasterized PNG frame attached to a compaction entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapFrame {
    /// Base64-encoded PNG bytes.
    pub png: String,
    pub width: u32,
    pub height: u32,
}

/// Versioned snapcompact payload stored under
/// `CompactionEntry.details[SNAPCOMPACT_DETAILS_KEY]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapPayload {
    pub schema: String,
    pub frames: Vec<SnapFrame>,
}

impl SnapPayload {
    #[must_use]
    pub fn new(frames: Vec<SnapFrame>) -> Self {
        Self {
            schema: SNAPCOMPACT_DETAILS_SCHEMA.to_string(),
            frames,
        }
    }
}

// ── Rendering ───────────────────────────────────────────────────────────────

fn normalize_char(c: char) -> char {
    match c {
        '\t' => ' ',
        c if (' '..='~').contains(&c) => c,
        _ => '?',
    }
}

/// Draw one normalized char into the RGB buffer at pixel `(x, y)`.
fn draw_glyph(buf: &mut [u8], width: u32, height: u32, x: u32, y: u32, c: char, color: Rgb) {
    let idx = (c as u32).wrapping_sub(0x20) as usize;
    let Some(glyph) = FONT_5X7.get(idx) else {
        return;
    };
    for (col, bits) in glyph.iter().enumerate() {
        for row in 0..GLYPH_H {
            if bits & (1 << row) == 0 {
                continue;
            }
            // Scale each font pixel to an NxN block.
            for sy in 0..SCALE {
                for sx in 0..SCALE {
                    let px = x + (col as u32) * SCALE + sx;
                    let py = y + row * SCALE + sy;
                    if px < width && py < height {
                        let o = ((py * width + px) * 3) as usize;
                        buf[o] = color[0];
                        buf[o + 1] = color[1];
                        buf[o + 2] = color[2];
                    }
                }
            }
        }
    }
}

/// Render `transcript` into one or more deterministic PNG frames.
///
/// Lines wrap at [`CHARS_PER_LINE`] columns; output taller than
/// [`MAX_FRAME_HEIGHT`] splits across frames at line boundaries. An empty
/// transcript produces no frames (callers stay text-only).
#[must_use]
pub fn render_frames(transcript: &str) -> Vec<SnapFrame> {
    let mut wrapped: Vec<(String, Rgb)> = Vec::new();
    for raw in transcript.lines() {
        let color = color_for_line(raw);
        let expanded = raw.replace('\t', "    ");
        if expanded.is_empty() {
            wrapped.push((String::new(), color));
            continue;
        }
        let chars: Vec<char> = expanded.chars().map(normalize_char).collect();
        for chunk in chars.chunks(CHARS_PER_LINE as usize) {
            wrapped.push((chunk.iter().collect(), color));
        }
    }

    let total_frames = wrapped.len().div_ceil(LINES_PER_FRAME as usize).max(1);
    let mut frames = Vec::with_capacity(total_frames);
    for group in wrapped.chunks(LINES_PER_FRAME as usize) {
        let height = PAD * 2 + (group.len() as u32) * LINE_HEIGHT;
        let mut buf = vec![0u8; (FRAME_WIDTH * height * 3) as usize];
        // Background fill.
        for px in buf.chunks_exact_mut(3) {
            px.copy_from_slice(&BACKGROUND);
        }
        for (li, (line, color)) in group.iter().enumerate() {
            let y = PAD + (li as u32) * LINE_HEIGHT;
            for (ci, ch) in line.chars().enumerate() {
                draw_glyph(
                    &mut buf,
                    FRAME_WIDTH,
                    height,
                    PAD + (ci as u32) * CHAR_ADVANCE,
                    y,
                    ch,
                    *color,
                );
            }
        }
        let png = png_encode(FRAME_WIDTH, height, &buf);
        frames.push(SnapFrame {
            png: base64::engine::general_purpose::STANDARD.encode(png),
            width: FRAME_WIDTH,
            height,
        });
    }
    frames
}

// ── PNG encoding ────────────────────────────────────────────────────────────

const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

fn crc32(data: &[u8]) -> u32 {
    // IEEE CRC-32 (bitwise, table-free): deterministic and dependency-free.
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

fn png_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// Encode 8-bit RGB pixels as a minimal, filter-0 PNG. Deterministic:
/// identical inputs yield byte-identical outputs.
#[must_use]
pub fn png_encode(width: u32, height: u32, rgb: &[u8]) -> Vec<u8> {
    debug_assert_eq!(rgb.len(), (width * height * 3) as usize);
    let mut out = Vec::with_capacity(rgb.len() + 1024);
    out.extend_from_slice(&PNG_SIGNATURE);

    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // depth 8, truecolor, no interlace
    png_chunk(&mut out, b"IHDR", &ihdr);

    // Raw scanlines: each row prefixed with filter byte 0 (None).
    let stride = (width * 3) as usize;
    let mut raw = Vec::with_capacity((stride + 1) * height as usize);
    for row in 0..height as usize {
        raw.push(0u8);
        raw.extend_from_slice(&rgb[row * stride..(row + 1) * stride]);
    }

    // zlib stream via flate2 (miniz_oxide backend: deterministic).
    let mut encoder =
        flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    let idat = std::io::Write::write_all(&mut encoder, &raw)
        .map(|()| encoder.finish().unwrap_or_default())
        .unwrap_or_default();
    png_chunk(&mut out, b"IDAT", &idat);
    png_chunk(&mut out, b"IEND", &[]);
    out
}

// ── Details integration ─────────────────────────────────────────────────────

/// Merge a snapcompact payload into existing compaction details JSON,
/// preserving all other keys (readFiles, modifiedFiles, mode, …).
#[must_use]
pub fn payload_to_details(existing: Option<serde_json::Value>, payload: &SnapPayload) -> serde_json::Value {
    use serde_json::Value;
    let mut details = match existing {
        Some(Value::Object(map)) => map,
        _ => serde_json::Map::new(),
    };
    if let Ok(v) = serde_json::to_value(payload) {
        details.insert(SNAPCOMPACT_DETAILS_KEY.to_string(), v);
    }
    Value::Object(details)
}

/// Extract validated snapcompact frames from compaction entry details.
/// Returns `None` when absent or when the schema tag does not match exactly
/// (fail-closed against foreign/malformed payloads).
#[must_use]
pub fn frames_from_details(details: Option<&serde_json::Value>) -> Option<SnapPayload> {
    let value = details?.get(SNAPCOMPACT_DETAILS_KEY)?;
    let payload: SnapPayload = serde_json::from_value(value.clone()).ok()?;
    if payload.schema != SNAPCOMPACT_DETAILS_SCHEMA {
        return None;
    }
    Some(payload)
}

/// Attach snapcompact frames to a freshly converted compaction-summary
/// message: content becomes `[Text(summary), Image(frame0), …]`. A message
/// without available frames is returned unchanged.
///
/// Timestamps are preserved from the incoming message so context rebuilds
/// stay deterministic relative to the source entry.
#[must_use]
pub fn attach_frames(mut message: Message, payload: Option<&SnapPayload>) -> Message {
    let Some(payload) = payload else {
        return message;
    };
    if payload.frames.is_empty() {
        return message;
    }
    if let Message::User(user) = &mut message {
        let mut blocks = match &user.content {
            UserContent::Text(text) => {
                vec![ContentBlock::Text(TextContent::new(text.text.clone()))]
            }
            UserContent::Blocks(existing) => existing.clone(),
        };
        for frame in &payload.frames {
            blocks.push(ContentBlock::Image(ImageContent {
                data: frame.png.clone(),
                mime_type: "image/png".to_string(),
            }));
        }
        user.content = UserContent::Blocks(blocks);
    }
    message
}

/// Outcome of vision gating over an outbound message list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StripStats {
    /// Image blocks removed from compaction summary messages.
    pub removed_frames: usize,
    /// Messages affected by removal.
    pub affected_messages: usize,
}

/// Strip snapcompact frames from compaction summary messages when the active
/// model cannot accept image inputs (bd-cv653.7.6 AC 4). Only messages whose
/// first block carries [`COMPACTION_SUMMARY_PREFIX`] are touched — user-pasted
/// images are never removed here.
///
/// Returns stats; logs one structured line per affected span with a stable
/// reason code so degradation is diagnosable from logs alone.
pub fn strip_snapcompact_images(messages: &mut [Message], accepts_images: bool) -> StripStats {
    let mut stats = StripStats::default();
    if accepts_images {
        return stats;
    }
    for message in messages.iter_mut() {
        let Message::User(user) = message else {
            continue;
        };
        let UserContent::Blocks(blocks) = &mut user.content else {
            continue;
        };
        let is_compaction_summary = blocks.first().is_some_and(|b| {
            matches!(b, ContentBlock::Text(t)
                if t.text.starts_with(COMPACTION_SUMMARY_PREFIX))
        });
        if !is_compaction_summary {
            continue;
        }
        let before = blocks.len();
        blocks.retain(|block| !matches!(block, ContentBlock::Image(_)));
        let removed = before - blocks.len();
        if removed > 0 {
            stats.removed_frames += removed;
            stats.affected_messages += 1;
        }
    }
    if stats.removed_frames > 0 {
        tracing::info!(
            target: "snapcompact",
            reason_code = "snapcompact_degraded_non_vision",
            removed_frames = stats.removed_frames,
            affected_messages = stats.affected_messages,
            "Active model lacks image input support; snapcompact frames stripped from outbound context"
        );
    }
    stats
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometry_constants_are_consistent() {
        assert!(CHARS_PER_LINE >= 40, "usable terminal width");
        assert!(LINES_PER_FRAME >= 10, "usable frame height");
        assert!(PAD * 2 < FRAME_WIDTH);
    }

    #[test]
    fn png_encode_produces_valid_structure() {
        let rgb = vec![255u8; 12 * 9 * 3];
        let png = png_encode(12, 9, &rgb);
        assert!(png.starts_with(&PNG_SIGNATURE));
        // IHDR length 13, type IHDR.
        assert_eq!(&png[8..12], &[0, 0, 0, 13]);
        assert_eq!(&png[12..16], b"IHDR");
        assert_eq!(png[16..20], 12u32.to_be_bytes());
        assert_eq!(png[20..24], 9u32.to_be_bytes());
        assert_eq!(&png[png.len() - 12..png.len() - 8], b"IEND");
    }

    #[test]
    fn strip_only_touches_compaction_summaries() {
        let summary_text =
            format!("{COMPACTION_SUMMARY_PREFIX}hello{COMPACTION_SUMMARY_SUFFIX}");
        let mut messages = vec![
            Message::User(UserMessage {
                content: UserContent::Blocks(vec![
                    ContentBlock::Text(TextContent::new(summary_text)),
                    ContentBlock::Image(ImageContent {
                        data: "AAA=".into(),
                        mime_type: "image/png".into(),
                    }),
                ]),
                timestamp: 0,
            }),
            Message::User(UserMessage {
                content: UserContent::Blocks(vec![ContentBlock::Image(ImageContent {
                    data: "BBB=".into(),
                    mime_type: "image/png".into(),
                })]),
                timestamp: 0,
            }),
        ];
        let stats = strip_snapcompact_images(&mut messages, false);
        assert_eq!(stats.removed_frames, 1);
        assert_eq!(stats.affected_messages, 1);
        let Message::User(user) = &messages[0] else {
            panic!("expected user message");
        };
        let UserContent::Blocks(blocks) = &user.content else {
            panic!("expected blocks");
        };
        assert_eq!(blocks.len(), 1, "only text remains on summary");
        let Message::User(other) = &messages[1] else {
            panic!("expected user message");
        };
        let UserContent::Blocks(other_blocks) = &other.content else {
            panic!("expected blocks");
        };
        assert_eq!(other_blocks.len(), 1, "user-pasted images untouched");

        // Vision-capable model: nothing stripped.
        let stats_ok = strip_snapcompact_images(&mut messages, true);
        assert_eq!(stats_ok, StripStats::default());
    }

    #[test]
    fn frames_roundtrip_through_details() {
        let frames = render_frames("[User]: hello\n[Assistant]: world\n");
        assert!(!frames.is_empty(), "transcript produces frames");
        let payload = SnapPayload::new(frames.clone());
        let details = payload_to_details(None, &payload);
        let extracted =
            frames_from_details(Some(&details)).expect("payload should extract");
        assert_eq!(extracted, payload);

        // Schema mismatch fails closed; missing details fail closed.
        let mut bad = details.clone();
        if let Some(obj) = bad.get_mut(SNAPCOMPACT_DETAILS_KEY) {
            if let Some(map) = obj.as_object_mut() {
                map.insert("schema".into(), serde_json::Value::String("other.v9".into()));
            }
        }
        assert!(frames_from_details(Some(&bad)).is_none());
        assert!(frames_from_details(None).is_none());
    }

    #[test]
    fn attach_frames_appends_image_blocks_after_text() {
        let base = Message::User(UserMessage {
            content: UserContent::Text("summary body".into()),
            timestamp: 42,
        });
        let payload = SnapPayload::new(render_frames("data"));
        let attached = attach_frames(base.clone(), Some(&payload));
        let Message::User(user) = attached else {
            panic!("expected user message");
        };
        let UserContent::Blocks(blocks) = &user.content else {
            panic!("expected blocks");
        };
        assert!(matches!(blocks[0], ContentBlock::Text(_)));
        assert_eq!(blocks.len(), 1 + payload.frames.len());
        assert!(
            blocks[1..]
                .iter()
                .all(|b| matches!(b, ContentBlock::Image(i) if i.mime_type == "image/png"))
        );
        assert_eq!(attach_frames(base.clone(), None), base, "no payload is a no-op");
    }

    #[test]
    fn render_is_deterministic_and_wraps() {
        let transcript = "[User]: repeat\n".repeat(3);
        let a = render_frames(&transcript);
        let b = render_frames(&transcript);
        assert_eq!(a, b, "identical transcripts render identical bytes");
        for frame in &a {
            assert_eq!(frame.width, FRAME_WIDTH);
            assert!(frame.height <= MAX_FRAME_HEIGHT);
        }
    }
}
