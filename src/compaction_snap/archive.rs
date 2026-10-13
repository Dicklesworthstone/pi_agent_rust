//! Bounded, streaming construction of the optional visual history archive.
//!
//! Limits count base64 bytes actually persisted and sent, not uncompressed
//! pixels or an estimated compression ratio. An archive is all-or-nothing:
//! returning a prefix as if it represented the whole compacted span would
//! silently lose history. Callers retain the ordinary text summary on error.

use super::{
    BACKGROUND, CHAR_ADVANCE, CHARS_PER_LINE, FRAME_WIDTH, LINE_HEIGHT, LINES_PER_FRAME, PAD, Rgb,
    SnapFrame, color_for_line, draw_glyph, normalize_char, png_encode,
};
use base64::Engine as _;

// Like OMP's renderer, keep small frames large enough for vision processors
// that reject very short images. This is padding, not extra transcript rows.
const MIN_FRAME_HEIGHT: u32 = 64;

/// Local archive limits, independent of a provider's whole-request limits.
///
/// Defaults bound local resource use, not provider acceptance. A caller that
/// knows its remaining image or byte allowance can supply a stricter budget.
/// The byte limit counts the base64 strings; JSON framing and other request
/// images need separate room. Higher official-endpoint limits are not assumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapLimits {
    pub max_frames: usize,
    pub max_encoded_bytes: usize,
}

impl Default for SnapLimits {
    fn default() -> Self {
        Self {
            max_frames: 90,
            max_encoded_bytes: 3 * 1024 * 1024,
        }
    }
}

/// Why a complete visual archive could not fit its allotted budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapRenderError {
    FrameLimit,
    ByteLimit,
}

impl SnapRenderError {
    #[must_use]
    pub const fn reason_code(self) -> &'static str {
        match self {
            Self::FrameLimit => "snapcompact_frame_limit",
            Self::ByteLimit => "snapcompact_byte_limit",
        }
    }
}

impl std::fmt::Display for SnapRenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::FrameLimit => "snapcompact archive exceeds its frame budget",
            Self::ByteLimit => "snapcompact archive exceeds its encoded-byte budget",
        })
    }
}

impl std::error::Error for SnapRenderError {}

/// Render a complete archive within explicit frame and base64-byte budgets.
///
/// Wrapping retains at most one frame of rows plus one short pending row,
/// even for a transcript consisting of one enormous line. Output allocations
/// depend on the budget, never on a precomputed transcript-wide line count.
/// Empty input succeeds with no frames, including when either limit is zero.
///
/// # Errors
///
/// Returns a typed limit error instead of a partial archive. Callers should
/// keep the text summary, not retry with an unbounded budget.
pub fn render_frames_with_limits(
    transcript: &str,
    limits: SnapLimits,
) -> Result<Vec<SnapFrame>, SnapRenderError> {
    let mut archive = ArchiveBuilder::new(limits);
    for raw in transcript.lines() {
        let color = color_for_line(raw);
        let mut row = String::with_capacity(CHARS_PER_LINE as usize);
        if raw.is_empty() {
            archive.push_row(row, color)?;
            continue;
        }
        for ch in raw.chars() {
            let (glyph, repetitions) = if ch == '\t' {
                (' ', 4)
            } else {
                (normalize_char(ch), 1)
            };
            for _ in 0..repetitions {
                row.push(glyph);
                // Normalization produces one ASCII byte per cell.
                if row.len() == CHARS_PER_LINE as usize {
                    archive.push_row(std::mem::take(&mut row), color)?;
                }
            }
        }
        if !row.is_empty() {
            archive.push_row(row, color)?;
        }
    }
    archive.finish()
}

struct ArchiveBuilder {
    limits: SnapLimits,
    frames: Vec<SnapFrame>,
    rows: Vec<(String, Rgb)>,
    encoded_bytes: usize,
}

impl ArchiveBuilder {
    fn new(limits: SnapLimits) -> Self {
        Self {
            limits,
            frames: Vec::new(),
            rows: Vec::with_capacity(LINES_PER_FRAME as usize),
            encoded_bytes: 0,
        }
    }

    fn push_row(&mut self, row: String, color: Rgb) -> Result<(), SnapRenderError> {
        if self.frames.len() >= self.limits.max_frames {
            return Err(SnapRenderError::FrameLimit);
        }
        if self.encoded_bytes >= self.limits.max_encoded_bytes {
            return Err(SnapRenderError::ByteLimit);
        }
        self.rows.push((row, color));
        if self.rows.len() == LINES_PER_FRAME as usize {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), SnapRenderError> {
        if self.rows.is_empty() {
            return Ok(());
        }
        if self.frames.len() >= self.limits.max_frames {
            return Err(SnapRenderError::FrameLimit);
        }
        let frame = render_group(&self.rows);
        if frame.png.len()
            > self
                .limits
                .max_encoded_bytes
                .saturating_sub(self.encoded_bytes)
        {
            return Err(SnapRenderError::ByteLimit);
        }
        // The subtraction check proves this addition cannot overflow.
        self.encoded_bytes += frame.png.len();
        self.frames.push(frame);
        self.rows.clear();
        Ok(())
    }

    fn finish(mut self) -> Result<Vec<SnapFrame>, SnapRenderError> {
        self.flush()?;
        Ok(self.frames)
    }
}

/// Geometry is bounded by the builder: at most LINES_PER_FRAME ASCII rows,
/// each at most CHARS_PER_LINE cells. Apart from padding short frames to the
/// vision minimum, keep the original rasterization and role colors unchanged.
#[allow(clippy::cast_possible_truncation)]
fn render_group(group: &[(String, Rgb)]) -> SnapFrame {
    let height = (PAD * 2 + u32::try_from(group.len()).unwrap_or(0) * LINE_HEIGHT)
        .max(MIN_FRAME_HEIGHT);
    let mut buf = vec![0u8; (FRAME_WIDTH * height * 3) as usize];
    for px in buf.as_chunks_mut::<3>().0 {
        *px = BACKGROUND;
    }
    for (li, (line, color)) in group.iter().enumerate() {
        let y = PAD + u32::try_from(li).unwrap_or(0) * LINE_HEIGHT;
        for (ci, ch) in line.chars().enumerate() {
            draw_glyph(
                &mut buf,
                FRAME_WIDTH,
                height,
                PAD + u32::try_from(ci).unwrap_or(0) * CHAR_ADVANCE,
                y,
                ch,
                *color,
            );
        }
    }
    SnapFrame {
        png: base64::engine::general_purpose::STANDARD.encode(png_encode(FRAME_WIDTH, height, &buf)),
        width: FRAME_WIDTH,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(frames: usize, bytes: usize) -> SnapLimits {
        SnapLimits {
            max_frames: frames,
            max_encoded_bytes: bytes,
        }
    }

    #[test]
    fn empty_input_needs_no_budget() {
        assert_eq!(render_frames_with_limits("", limits(0, 0)), Ok(Vec::new()));
        assert_eq!(
            render_frames_with_limits("a", limits(0, usize::MAX)),
            Err(SnapRenderError::FrameLimit)
        );
        assert_eq!(
            render_frames_with_limits("a", limits(1, 0)),
            Err(SnapRenderError::ByteLimit)
        );
    }

    #[test]
    fn exact_frame_boundary_fits_but_an_extra_row_fails_atomically() {
        let full = "a\n".repeat(LINES_PER_FRAME as usize);
        let one = limits(1, usize::MAX);
        assert_eq!(render_frames_with_limits(&full, one).unwrap().len(), 1);
        assert_eq!(
            render_frames_with_limits(&(full + "b"), one),
            Err(SnapRenderError::FrameLimit)
        );
    }

    #[test]
    fn wrapping_one_long_line_obeys_the_same_frame_limit() {
        let full = "x".repeat((CHARS_PER_LINE * LINES_PER_FRAME) as usize);
        let one = limits(1, usize::MAX);
        assert_eq!(render_frames_with_limits(&full, one).unwrap().len(), 1);
        assert_eq!(
            render_frames_with_limits(&(full + "x"), one),
            Err(SnapRenderError::FrameLimit)
        );
    }

    #[test]
    fn encoded_byte_boundary_is_inclusive_and_counts_base64() {
        let reference = render_frames_with_limits("a", SnapLimits::default()).unwrap();
        let size = reference[0].png.len();
        assert_eq!(
            render_frames_with_limits("a", limits(1, size)),
            Ok(reference)
        );
        assert_eq!(
            render_frames_with_limits("a", limits(1, size - 1)),
            Err(SnapRenderError::ByteLimit)
        );
    }

    #[test]
    fn byte_budget_is_shared_by_all_frames_not_reset_per_frame() {
        let text = "row\n".repeat(LINES_PER_FRAME as usize + 1);
        let reference = render_frames_with_limits(&text, limits(2, usize::MAX)).unwrap();
        assert_eq!(reference.len(), 2);
        let total = reference.iter().map(|frame| frame.png.len()).sum::<usize>();
        assert_eq!(
            render_frames_with_limits(&text, limits(2, total)),
            Ok(reference)
        );
        assert_eq!(
            render_frames_with_limits(&text, limits(2, total - 1)),
            Err(SnapRenderError::ByteLimit)
        );
    }

    #[test]
    fn rows_keep_original_roles_blank_lines_and_crlf_behavior() {
        let transcript = "[User]: hi\r\n\r\n[Assistant]: bye\n";
        let expected = render_group(&[
            ("[User]: hi".to_string(), color_for_line("[User]: hi")),
            (String::new(), color_for_line("")),
            (
                "[Assistant]: bye".to_string(),
                color_for_line("[Assistant]: bye"),
            ),
        ]);
        assert_eq!(
            render_frames_with_limits(transcript, SnapLimits::default()),
            Ok(vec![expected])
        );
    }

    #[test]
    fn tabs_cross_wrap_boundaries_without_losing_spaces() {
        let prefix = "x".repeat(CHARS_PER_LINE as usize - 2);
        assert_eq!(
            render_frames_with_limits(&format!("{prefix}\tend"), SnapLimits::default()),
            render_frames_with_limits(&format!("{prefix}    end"), SnapLimits::default())
        );
    }

    #[test]
    fn small_frames_have_a_vision_compatible_minimum_height() {
        let frames = render_frames_with_limits("a", SnapLimits::default()).unwrap();
        assert_eq!(frames[0].height, MIN_FRAME_HEIGHT);
        let png = base64::engine::general_purpose::STANDARD
            .decode(&frames[0].png)
            .unwrap();
        assert_eq!(&png[20..24], &MIN_FRAME_HEIGHT.to_be_bytes());
    }

    #[test]
    fn production_entry_point_uses_bounded_renderer() {
        assert_eq!(
            super::super::render_frames("[User]: hello"),
            render_frames_with_limits("[User]: hello", SnapLimits::default()).unwrap()
        );
        // Empty rows still consume frames. The wrapper must keep the text-only
        // fallback instead of publishing an incomplete visual history prefix.
        let oversized =
            "\n".repeat(LINES_PER_FRAME as usize * SnapLimits::default().max_frames + 1);
        assert!(super::super::render_frames(&oversized).is_empty());
    }
}
