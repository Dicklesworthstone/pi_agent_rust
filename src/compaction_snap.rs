//! Snapcompact compaction mode (bd-cv653.7.6).
//!
//! Experimental context-compression mode: alongside the text summary, the
//! compacted conversation span is rasterized into deterministic PNG frames
//! ("terminal-style" bitmap rendering) and attached to the compaction
//! summary message as image blocks. Vision-capable models can then consume
//! the rasterized history; text-only models receive the retained source text
//! instead. The stored archive is independent of the selected model.
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
//! - Archive construction is streaming and bounded. If the complete visual
//!   history exceeds its budget, the ordinary text summary remains authoritative.

use crate::model::{
    ContentBlock, CustomMessage, ImageContent, Message, TextContent, UserContent, UserMessage,
};
use serde::{Deserialize, Serialize};

mod archive;
mod replay;
pub use archive::{SnapLimits, SnapRenderError, render_frames_with_limits};

/// Schema tag stored with the frames inside `CompactionEntry.details`.
pub const SNAPCOMPACT_DETAILS_SCHEMA: &str = "pi.compaction.snapcompact.v1";

/// Key under `CompactionEntry.details` holding the snapcompact payload.
pub const SNAPCOMPACT_DETAILS_KEY: &str = "snapcompact";

/// Native, model-independent carrier used only while rebuilding context.
/// It is projected to an ordinary user message before a provider request.
pub(crate) const SNAPCOMPACT_MESSAGE_TYPE: &str = "pi.snapcompact";

/// A complete source archive is retained or rejected; it is never silently
/// clipped while being saved. This allowance is separate from encoded images.
pub const MAX_SOURCE_TEXT_BYTES: usize = 3 * 1024 * 1024;

/// Bare SDK agents have no model window metadata. Session-owned agents replace
/// this conservative allowance with half their available compaction window.
pub(crate) const DEFAULT_SOURCE_TEXT_TOKENS: u32 = 8_192;

const SOURCE_TEXT_PREFIX: &str = "\n\n<compacted_history>\n";
const SOURCE_TEXT_SUFFIX: &str = "\n</compacted_history>";

/// Prefix used on compaction summary messages (mirrors `session.rs`).
///
/// Identifies which user messages carry snapcompact frames so stripping only
/// ever touches our own image blocks, never user-pasted images.
pub const COMPACTION_SUMMARY_PREFIX: &str = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";

/// Suffix paired with [`COMPACTION_SUMMARY_PREFIX`].
pub const COMPACTION_SUMMARY_SUFFIX: &str = "\n</summary>";
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
    /// The privacy-screened transcript used to render these frames. Archives
    /// written before source retention have no recoverable text beyond their
    /// separately stored summary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_text: Option<String>,
}

impl SnapPayload {
    #[must_use]
    pub fn new(frames: Vec<SnapFrame>) -> Self {
        Self {
            schema: SNAPCOMPACT_DETAILS_SCHEMA.to_string(),
            frames,
            source_text: None,
        }
    }

    /// Retain the same already-screened source that was rasterized. Persistence
    /// and attachment validate the source budget before accepting the archive.
    #[must_use]
    pub fn with_source_text(mut self, source_text: String) -> Self {
        self.source_text = Some(source_text);
        self
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
///
/// Casts are bounded: glyph indices are masked to the 95-entry table and
/// pixel coordinates are derived from module geometry constants.
#[allow(clippy::cast_possible_truncation)]
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
/// transcript produces no frames. Archives exceeding [`SnapLimits::default`]
/// also produce no frames, with a logged reason; callers retain the complete
/// text summary rather than attaching a misleading partial visual archive.
#[must_use]
pub fn render_frames(transcript: &str) -> Vec<SnapFrame> {
    match render_frames_with_limits(transcript, SnapLimits::default()) {
        Ok(frames) => frames,
        Err(error) => {
            tracing::warn!(
                target: "snapcompact",
                reason_code = error.reason_code(),
                "Visual archive exceeds its budget; retaining the text-only compaction summary"
            );
            Vec::new()
        }
    }
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

#[allow(clippy::cast_possible_truncation)] // PNG chunk lengths are < u32::MAX by construction
fn png_chunk(out: &mut Vec<u8>, kind: [u8; 4], data: &[u8]) {
    let len = u32::try_from(data.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&len.to_be_bytes());
    let start = out.len();
    out.extend_from_slice(&kind);
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
    png_chunk(&mut out, *b"IHDR", &ihdr);

    // Raw scanlines: each row prefixed with filter byte 0 (None).
    let stride = (width * 3) as usize;
    let mut raw = Vec::with_capacity((stride + 1) * height as usize);
    for row in 0..height as usize {
        raw.push(0u8);
        raw.extend_from_slice(&rgb[row * stride..(row + 1) * stride]);
    }

    // zlib stream via flate2 (miniz_oxide backend: deterministic).
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    let idat = std::io::Write::write_all(&mut encoder, &raw)
        .map(|()| encoder.finish().unwrap_or_default())
        .unwrap_or_default();
    png_chunk(&mut out, *b"IDAT", &idat);
    png_chunk(&mut out, *b"IEND", &[]);
    out
}

// ── Details integration ─────────────────────────────────────────────────────

/// Merge a snapcompact payload into existing compaction details JSON,
/// preserving all other keys (readFiles, modifiedFiles, mode, …).
///
/// Invalid or over-budget archives remove any stale snapcompact value;
/// unrelated details and the separately stored text summary are preserved.
#[must_use]
pub fn payload_to_details(
    existing: Option<serde_json::Value>,
    payload: &SnapPayload,
) -> serde_json::Value {
    use serde_json::Value;
    let mut details = match existing {
        Some(Value::Object(map)) => map,
        _ => serde_json::Map::new(),
    };
    if let Err(error) = replay::validate_payload(payload, SnapLimits::default()) {
        tracing::warn!(
            target: "snapcompact",
            reason_code = error.reason_code(),
            "Visual archive was not persisted; retaining the text-only compaction summary"
        );
        details.remove(SNAPCOMPACT_DETAILS_KEY);
        return Value::Object(details);
    }
    if let Ok(v) = serde_json::to_value(payload) {
        details.insert(SNAPCOMPACT_DETAILS_KEY.to_string(), v);
    }
    Value::Object(details)
}

/// Extract an admitted archive from compaction entry details.
///
/// Invalid frames may still retain a valid bounded source-only reading.
/// Returns `None` when neither reading is admissible.
/// Known fields are borrowed until the complete archive passes count, byte,
/// geometry and native PNG integrity checks; unknown JSON fields are not cloned.
#[must_use]
pub fn frames_from_details(details: Option<&serde_json::Value>) -> Option<SnapPayload> {
    let value = details?.get(SNAPCOMPACT_DETAILS_KEY)?;
    match replay::parse_payload(value, SnapLimits::default()) {
        Ok(payload) => Some(payload),
        Err(error) => {
            let source = source_from_details(details);
            tracing::warn!(
                target: "snapcompact",
                reason_code = error.reason_code(),
                retained_source = source.is_some(),
                "Stored visual archive was rejected; retaining available text"
            );
            source.map(|source| SnapPayload::new(Vec::new()).with_source_text(source.to_string()))
        }
    }
}

/// Attach an archive to a freshly converted compaction-summary message.
/// Source-bearing archives use a native custom-message envelope until the
/// request chooses text or images. Frame-only archives retain the ordinary
/// `[Text(summary), Image(frame0), …]` form. No provider receives the envelope.
///
/// Timestamps are preserved from the incoming message so context rebuilds
/// stay deterministic relative to the source entry. Direct SDK payloads are
/// validated too. Reattaching the same complete archive is a no-op; identical
/// frames within an archive are not collapsed, since they may encode repeated
/// history.
#[must_use]
pub fn attach_frames(mut message: Message, payload: Option<&SnapPayload>) -> Message {
    let Some(payload) = payload else {
        return message;
    };
    if (payload.frames.is_empty() && payload.source_text.is_none())
        || !matches!(&message, Message::User(_))
    {
        return message;
    }
    if let Err(error) = replay::validate_payload(payload, SnapLimits::default()) {
        tracing::warn!(
            target: "snapcompact",
            reason_code = error.reason_code(),
            "Visual archive was not attached; retaining the original summary message"
        );
        return message;
    }
    if let Message::User(user) = &mut message {
        if payload.source_text.is_some() {
            let summary = match &user.content {
                UserContent::Text(text) => Some(text.as_str()),
                UserContent::Blocks(blocks) => match blocks.as_slice() {
                    [ContentBlock::Text(text)] => Some(text.text.as_str()),
                    _ => None,
                },
            };
            if let Some(summary) = summary {
                return Message::Custom(CustomMessage {
                    content: summary.to_string(),
                    custom_type: SNAPCOMPACT_MESSAGE_TYPE.to_string(),
                    display: true,
                    details: Some(payload_to_details(None, payload)),
                    timestamp: user.timestamp,
                });
            }
            // Source-bearing archives must keep their typed carrier. Never
            // drop the source and append independent unscreened image bytes
            // when an SDK caller supplies an unsupported content shape.
            return message;
        }
        if let UserContent::Blocks(existing) = &user.content
            && has_archive_suffix(existing, &payload.frames)
        {
            return message;
        }
        let mut blocks = match &user.content {
            UserContent::Text(text) => {
                vec![ContentBlock::Text(TextContent::new(text.clone()))]
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

/// Whether this message needs native archive projection, including on vision
/// models. Recognition does not authorize unvalidated archive data.
#[must_use]
pub(crate) fn is_archive_message(message: &Message) -> bool {
    matches!(message, Message::Custom(custom) if custom.custom_type == SNAPCOMPACT_MESSAGE_TYPE)
}

/// Retained source for a text-only consumer. The source has its own bounded
/// shape check; a reader that sends no images need not decompress any PNG.
/// The caller must apply its usual outbound privacy and text-budget policy.
#[must_use]
pub(crate) fn source_for_custom(custom: &CustomMessage) -> Option<&str> {
    if custom.custom_type != SNAPCOMPACT_MESSAGE_TYPE {
        return None;
    }
    source_from_details(custom.details.as_ref())
}

#[must_use]
pub(crate) fn source_from_details(details: Option<&serde_json::Value>) -> Option<&str> {
    let payload = details?.get(SNAPCOMPACT_DETAILS_KEY)?;
    if payload.get("schema").and_then(serde_json::Value::as_str) != Some(SNAPCOMPACT_DETAILS_SCHEMA)
    {
        return None;
    }
    replay::source_text(payload).ok().flatten()
}

/// Build a text-only reading without losing information that was kept only in
/// images. The summary remains first; the archive is labeled as past history.
#[must_use]
pub(crate) fn text_for_custom(custom: &CustomMessage) -> String {
    let mut text = custom.content.clone();
    if let Some(source) = source_for_custom(custom) {
        text.push_str(SOURCE_TEXT_PREFIX);
        text.push_str(source);
        text.push_str(SOURCE_TEXT_SUFFIX);
    }
    text
}

/// Update only the outbound copy after privacy screening. Vision projection
/// derives its raster from this admitted source, so stale persisted image bytes
/// cannot bypass the current privacy policy.
pub(crate) fn replace_source_text(custom: &mut CustomMessage, source: String) {
    if let Some(value) = custom
        .details
        .as_mut()
        .and_then(|details| details.get_mut(SNAPCOMPACT_DETAILS_KEY))
        .and_then(|payload| payload.get_mut("sourceText"))
    {
        *value = serde_json::Value::String(source);
    }
}

fn budgeted_source(source: &str, budget: Option<u32>, provider: &str, model_id: &str) -> String {
    let Some(budget) = budget else {
        return source.to_string();
    };
    let budget = u64::from(budget);
    let source_tokens = crate::token_count::count_tokens_for_model(source, provider, model_id);
    if source_tokens <= budget {
        return source.to_string();
    }
    const OMITTED: &str =
        "\n[Middle of compacted history omitted to fit this model's text budget.]\n";
    // Recount the actual retained ends because their token density can differ
    // from the middle. At most 22 halvings for the bounded 3-MiB source.
    let mut kept = usize::try_from(
        u64::try_from(source.len())
            .unwrap_or(u64::MAX)
            .saturating_mul(budget)
            / source_tokens,
    )
    .unwrap_or(source.len())
    .min(source.len().saturating_sub(1));
    while kept > 0 {
        let mut head = kept.div_ceil(2);
        while !source.is_char_boundary(head) {
            head -= 1;
        }
        let mut tail = source.len().saturating_sub(kept / 2);
        while !source.is_char_boundary(tail) {
            tail += 1;
        }
        let candidate = format!("{}{OMITTED}{}", &source[..head], &source[tail..]);
        if crate::token_count::count_tokens_for_model(&candidate, provider, model_id) <= budget {
            return candidate;
        }
        kept /= 2;
    }
    for notice in [
        "[Compacted history omitted: this model has no room for its text reading.]",
        "[Compacted history omitted.]",
        "[omitted]",
    ] {
        if crate::token_count::count_tokens_for_model(notice, provider, model_id) <= budget {
            return notice.to_string();
        }
    }
    String::new()
}

fn project_archive(
    custom: &CustomMessage,
    accepts_images: bool,
    text_budget: Option<u32>,
    provider: &str,
    model_id: &str,
) -> Message {
    let source = source_for_custom(custom);
    let base = Message::User(UserMessage {
        content: UserContent::Text(if accepts_images {
            custom.content.clone()
        } else {
            source.map_or_else(
                || custom.content.clone(),
                |source| {
                    let reading = budgeted_source(source, text_budget, provider, model_id);
                    if reading.is_empty() {
                        return custom.content.clone();
                    }
                    format!(
                        "{}{SOURCE_TEXT_PREFIX}{reading}{SOURCE_TEXT_SUFFIX}",
                        custom.content
                    )
                },
            )
        }),
        timestamp: custom.timestamp,
    });
    if !accepts_images {
        return base;
    }
    if let Some(source) = source {
        // Source is the privacy authority. Integrity-valid stored PNGs could
        // still depict unrelated unscreened text after an SDK edit or session
        // corruption; derive the reading from exactly the admitted source.
        let frames = render_frames(source);
        if frames.is_empty() {
            return project_archive(custom, false, text_budget, provider, model_id);
        }
        return attach_frames(base, Some(&SnapPayload::new(frames)));
    }
    let Some(mut payload) = frames_from_details(custom.details.as_ref()) else {
        return base;
    };
    // The provider gets one reading, never the metadata or a second copy of
    // the transcript. The canonical envelope still owns both representations.
    payload.source_text = None;
    attach_frames(base, Some(&payload))
}

fn has_archive_suffix(blocks: &[ContentBlock], frames: &[SnapFrame]) -> bool {
    let Some(start) = blocks.len().checked_sub(frames.len()) else {
        return false;
    };
    blocks[start..].iter().zip(frames).all(|(block, frame)| {
        matches!(block, ContentBlock::Image(image)
            if image.mime_type == "image/png" && image.data == frame.png)
    })
}

/// Outcome of vision gating over an outbound message list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StripStats {
    /// Image blocks removed from compaction summary messages.
    pub removed_frames: usize,
    /// Messages affected by removal.
    pub affected_messages: usize,
}

/// Project native archives for this request and strip legacy frames when the
/// target cannot read images. Call on an outbound copy, not durable history.
///
/// Strips when the active model cannot accept image inputs (bd-cv653.7.6 AC 4).
/// Only native archive envelopes and legacy messages whose first block carries
/// [`COMPACTION_SUMMARY_PREFIX`] are touched. User-pasted images are preserved.
///
/// Returns stats; logs one structured line per affected span with a stable
/// reason code so degradation is diagnosable from logs alone.
pub fn strip_snapcompact_images(messages: &mut [Message], accepts_images: bool) -> StripStats {
    project_for_model(messages, accepts_images, None, "", "")
}

/// Select one archive reading after full-source privacy screening. A source
/// budget clips only the outbound copy and labels any omitted middle.
pub(crate) fn project_for_model(
    messages: &mut [Message],
    accepts_images: bool,
    text_budget: Option<u32>,
    provider: &str,
    model_id: &str,
) -> StripStats {
    let mut stats = StripStats::default();
    for message in messages.iter_mut() {
        if let Message::Custom(custom) = message
            && custom.custom_type == SNAPCOMPACT_MESSAGE_TYPE
        {
            if !accepts_images {
                stats.affected_messages += 1;
                stats.removed_frames += custom
                    .details
                    .as_ref()
                    .and_then(|details| details.get(SNAPCOMPACT_DETAILS_KEY))
                    .and_then(|payload| payload.get("frames"))
                    .and_then(serde_json::Value::as_array)
                    .map_or(0, Vec::len);
            }
            *message = project_archive(custom, accepts_images, text_budget, provider, model_id);
        }
        if accepts_images {
            continue;
        }
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
            "Snapcompact uses retained text for this request; frame-only archives retain their summary"
        );
    }
    stats
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::UserMessage;

    #[test]
    fn geometry_constants_are_consistent() {
        assert!(CHARS_PER_LINE >= 40, "usable terminal width");
        assert!(LINES_PER_FRAME >= 10, "usable frame height");
        assert!(PAD * 2 < FRAME_WIDTH);
    }

    #[test]
    fn text_budget_preserves_unicode_ends_and_respects_even_tiny_allowances() {
        let source = format!(
            "HEAD-FACT {}MIDDLE-FACT {}TAIL-FACT",
            "caf\u{e9} \u{65e5}\u{672c} ".repeat(500),
            "caf\u{e9} \u{65e5}\u{672c} ".repeat(500),
        );
        for budget in [0, 1, 4, 64, 128] {
            let reading = budgeted_source(&source, Some(budget), "openai", "gpt-4o");
            assert!(crate::token_count::count_tokens(&reading, "openai") <= u64::from(budget));
            if budget >= 64 {
                assert!(reading.contains("HEAD-FACT") && reading.contains("TAIL-FACT"));
                assert!(!reading.contains("MIDDLE-FACT"));
                assert!(reading.contains("omitted"));
            }
        }
        assert_eq!(budgeted_source(&source, None, "openai", "gpt-4o"), source);
    }

    #[test]
    fn archive_text_budget_uses_the_destination_model_family() {
        let source = "お誕生日おめでとう source/path.rs <|endoftext|>\n".repeat(500);
        for model in ["gpt-4-turbo", "gpt-4o", "vendor/gpt-3.5-turbo"] {
            for budget in [0, 1, 64, 256] {
                let reading = budgeted_source(&source, Some(budget), "gateway", model);
                assert!(
                    crate::token_count::count_tokens_for_model(&reading, "gateway", model)
                        <= u64::from(budget),
                    "{model} must use one table for clipping and recounting",
                );
            }
        }
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
        assert_eq!(&png[png.len() - 12..png.len() - 8], &[0, 0, 0, 0]);
        assert_eq!(&png[png.len() - 8..png.len() - 4], b"IEND");
    }

    #[test]
    fn strip_only_touches_compaction_summaries() {
        let summary_text = format!("{COMPACTION_SUMMARY_PREFIX}hello{COMPACTION_SUMMARY_SUFFIX}");
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
        let payload = SnapPayload::new(frames);
        let details = payload_to_details(None, &payload);
        let extracted = frames_from_details(Some(&details)).expect("payload should extract");
        assert_eq!(extracted, payload);

        // Schema mismatch fails closed; missing details fail closed.
        let mut bad = details;
        if let Some(obj) = bad.get_mut(SNAPCOMPACT_DETAILS_KEY) {
            if let Some(map) = obj.as_object_mut() {
                map.insert(
                    "schema".into(),
                    serde_json::Value::String("other.v9".into()),
                );
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
        // No payload → untouched message (checked before `base` is moved).
        if let Message::User(u) = attach_frames(base.clone(), None) {
            assert!(
                matches!(&u.content, UserContent::Text(t) if t == "summary body"),
                "no payload is a no-op"
            );
        }

        let payload = SnapPayload::new(render_frames("data"));
        let attached = attach_frames(
            Message::User(UserMessage {
                content: UserContent::Text("summary body".into()),
                timestamp: 42,
            }),
            Some(&payload),
        );
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
        if let Message::User(u) = attach_frames(base, None) {
            assert!(
                matches!(&u.content, UserContent::Text(t) if t == "summary body"),
                "no payload is a no-op"
            );
        }
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
