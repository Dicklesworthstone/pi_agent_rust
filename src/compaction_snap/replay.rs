//! Admit a complete persisted archive before cloning or attaching any frame.
//!
//! The v1 producer writes a minimal RGB PNG: one IHDR, one IDAT and one IEND.
//! Validate that format, dimensions, checksums and the complete zlib stream.
//! Decompression uses a fixed buffer and stops at the declared pixel budget;
//! neither a forged header nor compressed input controls an output allocation.

use super::{
    FRAME_WIDTH, MAX_FRAME_HEIGHT, PNG_SIGNATURE, SNAPCOMPACT_DETAILS_SCHEMA, SnapFrame,
    SnapLimits, SnapPayload, crc32,
};
use base64::Engine as _;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReplayError {
    Shape,
    Schema,
    FrameLimit,
    ByteLimit,
    Geometry,
    Encoding,
    Png,
}

impl ReplayError {
    pub(super) const fn reason_code(self) -> &'static str {
        match self {
            Self::Shape => "snapcompact_invalid_payload",
            Self::Schema => "snapcompact_unknown_schema",
            Self::FrameLimit => "snapcompact_frame_limit",
            Self::ByteLimit => "snapcompact_byte_limit",
            Self::Geometry => "snapcompact_invalid_geometry",
            Self::Encoding => "snapcompact_invalid_base64",
            Self::Png => "snapcompact_invalid_png",
        }
    }
}

#[derive(Clone, Copy)]
struct FrameView<'a> {
    png: &'a str,
    width: u32,
    height: u32,
}

impl<'a> FrameView<'a> {
    fn from_value(value: &'a Value) -> Result<Self, ReplayError> {
        let dimension = |name: &str| {
            value
                .get(name)
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok())
                .ok_or(ReplayError::Geometry)
        };
        Ok(Self {
            png: value
                .get("png")
                .and_then(Value::as_str)
                .ok_or(ReplayError::Shape)?,
            width: dimension("width")?,
            height: dimension("height")?,
        })
    }

    const fn from_frame(frame: &'a SnapFrame) -> Self {
        Self {
            png: frame.png.as_str(),
            width: frame.width,
            height: frame.height,
        }
    }
}

/// Borrow only the schema's known fields. In particular, do not clone the
/// entire details Value (which may contain arbitrarily large unknown fields)
/// or a frame's base64 string until the whole archive has been admitted.
pub(super) fn parse_payload(value: &Value, limits: SnapLimits) -> Result<SnapPayload, ReplayError> {
    if value.get("schema").and_then(Value::as_str) != Some(SNAPCOMPACT_DETAILS_SCHEMA) {
        return Err(ReplayError::Schema);
    }
    let raw = value
        .get("frames")
        .and_then(Value::as_array)
        .ok_or(ReplayError::Shape)?;
    if raw.len() > limits.max_frames {
        return Err(ReplayError::FrameLimit);
    }
    let views = raw
        .iter()
        .map(FrameView::from_value)
        .collect::<Result<Vec<_>, _>>()?;
    validate_views(&views, limits)?;
    Ok(SnapPayload::new(
        views
            .into_iter()
            .map(|frame| SnapFrame {
                png: frame.png.to_owned(),
                width: frame.width,
                height: frame.height,
            })
            .collect(),
    ))
}

/// Public payload fields also permit direct SDK construction, so attachment
/// and persistence must not rely on callers having used the JSON reader.
pub(super) fn validate_payload(
    payload: &SnapPayload,
    limits: SnapLimits,
) -> Result<(), ReplayError> {
    if payload.schema != SNAPCOMPACT_DETAILS_SCHEMA {
        return Err(ReplayError::Schema);
    }
    if payload.frames.len() > limits.max_frames {
        return Err(ReplayError::FrameLimit);
    }
    let views = payload
        .frames
        .iter()
        .map(FrameView::from_frame)
        .collect::<Vec<_>>();
    validate_views(&views, limits)
}

fn validate_views(views: &[FrameView<'_>], limits: SnapLimits) -> Result<(), ReplayError> {
    let mut remaining = limits.max_encoded_bytes;
    // Admit all dimensions and bytes before decoding even the first frame.
    for frame in views {
        if frame.width == 0
            || frame.width > FRAME_WIDTH
            || frame.height == 0
            || frame.height > MAX_FRAME_HEIGHT
        {
            return Err(ReplayError::Geometry);
        }
        remaining = remaining
            .checked_sub(frame.png.len())
            .ok_or(ReplayError::ByteLimit)?;
    }
    for frame in views {
        let png = base64::engine::general_purpose::STANDARD
            .decode(frame.png)
            .map_err(|_| ReplayError::Encoding)?;
        let idat = native_png_idat(&png, frame.width, frame.height).ok_or(ReplayError::Png)?;
        if !valid_scanlines(idat, frame.width, frame.height) {
            return Err(ReplayError::Png);
        }
    }
    Ok(())
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let end = offset.checked_add(4)?;
    Some(u32::from_be_bytes(bytes.get(offset..end)?.try_into().ok()?))
}

/// Recognize only the minimal PNG format actually written by the v1 producer.
/// The returned IDAT borrows the decoded bytes; no second compressed copy is
/// needed. Unknown chunks and trailing data are not silently ignored.
fn native_png_idat(png: &[u8], width: u32, height: u32) -> Option<&[u8]> {
    if png.get(..8)? != PNG_SIGNATURE
        || read_u32(png, 8)? != 13
        || png.get(12..16)? != b"IHDR"
        || read_u32(png, 16)? != width
        || read_u32(png, 20)? != height
        || png.get(24..29)? != [8, 2, 0, 0, 0]
        || read_u32(png, 29)? != crc32(png.get(12..29)?)
        || png.get(37..41)? != b"IDAT"
    {
        return None;
    }
    let length = usize::try_from(read_u32(png, 33)?).ok()?;
    let end = 41usize.checked_add(length)?;
    if end.checked_add(16)? != png.len()
        || read_u32(png, end)? != crc32(png.get(37..end)?)
        || read_u32(png, end + 4)? != 0
        || png.get(end + 8..end + 12)? != b"IEND"
        || read_u32(png, end + 12)? != crc32(b"IEND")
    {
        return None;
    }
    png.get(41..end)
}

/// Requiring StreamEnd matters: an ordinary read-to-EOF is not evidence that
/// a truncated compressed stream included its final checksum. Also reject a
/// second stream or garbage after the first, invalid filters and excess rows.
fn valid_scanlines(idat: &[u8], width: u32, height: u32) -> bool {
    let stride = u64::from(width) * 3 + 1;
    let expected = stride * u64::from(height);
    let Ok(step) = usize::try_from(stride) else {
        return false;
    };
    let mut decoder = flate2::Decompress::new(true);
    let mut buffer = [0u8; 8192];
    loop {
        let before_in = decoder.total_in();
        let before_out = decoder.total_out();
        let Ok(offset) = usize::try_from(before_in) else {
            return false;
        };
        let Some(input) = idat.get(offset..) else {
            return false;
        };
        let Ok(status) = decoder.decompress(input, &mut buffer, flate2::FlushDecompress::None)
        else {
            return false;
        };
        let after_in = decoder.total_in();
        let after_out = decoder.total_out();
        let Ok(written) = usize::try_from(after_out - before_out) else {
            return false;
        };
        if after_out > expected || written > buffer.len() {
            return false;
        }
        let Ok(first_filter) = usize::try_from((stride - before_out % stride) % stride) else {
            return false;
        };
        if first_filter < written
            && buffer[first_filter..written]
                .iter()
                .step_by(step)
                .any(|filter| *filter > 4)
        {
            return false;
        }
        if status == flate2::Status::StreamEnd {
            return after_out == expected && after_in == idat.len() as u64;
        }
        if after_in == before_in && after_out == before_out {
            return false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compaction_snap::{
        SNAPCOMPACT_DETAILS_KEY, attach_frames, frames_from_details, payload_to_details, png_chunk,
        png_encode,
    };
    use crate::model::{
        ContentBlock, ImageContent, Message, TextContent, UserContent, UserMessage,
    };
    use serde_json::json;
    use std::io::Write;

    fn frame(width: u32, height: u32) -> SnapFrame {
        SnapFrame {
            png: base64::engine::general_purpose::STANDARD.encode(png_encode(
                width,
                height,
                &vec![42u8; (width * height * 3) as usize],
            )),
            width,
            height,
        }
    }

    fn bytes(frame: &SnapFrame) -> Vec<u8> {
        base64::engine::general_purpose::STANDARD
            .decode(&frame.png)
            .unwrap()
    }

    fn with_bytes(frame: &SnapFrame, png: &[u8]) -> SnapFrame {
        SnapFrame {
            png: base64::engine::general_purpose::STANDARD.encode(png),
            ..frame.clone()
        }
    }

    fn with_idat(frame: &SnapFrame, idat: &[u8]) -> SnapFrame {
        let mut png = bytes(frame)[..33].to_vec();
        png_chunk(&mut png, *b"IDAT", idat);
        png_chunk(&mut png, *b"IEND", &[]);
        with_bytes(frame, &png)
    }

    fn compress(raw: &[u8]) -> Vec<u8> {
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(raw).unwrap();
        encoder.finish().unwrap()
    }

    fn validate(frame: SnapFrame) -> Result<(), ReplayError> {
        validate_payload(&SnapPayload::new(vec![frame]), SnapLimits::default())
    }

    fn summary() -> Message {
        Message::User(UserMessage {
            content: UserContent::Text("complete text summary".into()),
            timestamp: 42,
        })
    }

    #[test]
    fn native_frames_and_pre_padding_legacy_frames_roundtrip() {
        let payload = SnapPayload::new(vec![frame(1, 1), frame(FRAME_WIDTH, 32)]);
        let details = payload_to_details(Some(json!({"readFiles":["src/lib.rs"]})), &payload);
        assert_eq!(frames_from_details(Some(&details)), Some(payload));
        assert_eq!(details["readFiles"], json!(["src/lib.rs"]));
    }

    #[test]
    fn empty_archive_is_valid_but_missing_or_foreign_schema_is_not() {
        let empty = SnapPayload::new(Vec::new());
        assert!(validate_payload(&empty, SnapLimits::default()).is_ok());
        assert_eq!(
            parse_payload(
                &json!({"schema":"foreign","frames":[]}),
                SnapLimits::default()
            ),
            Err(ReplayError::Schema)
        );
        assert!(frames_from_details(None).is_none());
        assert!(frames_from_details(Some(&json!({}))).is_none());
    }

    #[test]
    fn malformed_shapes_and_non_integer_dimensions_fail_closed() {
        let valid = serde_json::to_value(frame(1, 1)).unwrap();
        for field in ["png", "width", "height"] {
            for bad in [Value::Null, json!(false), json!(-1), json!(1.5), json!({})] {
                let mut value = valid.clone();
                value[field] = bad;
                assert!(
                    parse_payload(
                        &json!({"schema":SNAPCOMPACT_DETAILS_SCHEMA,"frames":[value]}),
                        SnapLimits::default(),
                    )
                    .is_err()
                );
            }
        }
        assert!(
            parse_payload(
                &json!({"schema":SNAPCOMPACT_DETAILS_SCHEMA,"frames":{}}),
                SnapLimits::default(),
            )
            .is_err()
        );
    }

    #[test]
    fn frame_count_is_checked_before_inspecting_elements() {
        assert_eq!(
            parse_payload(
                &json!({"schema":SNAPCOMPACT_DETAILS_SCHEMA,"frames":[null,null]}),
                SnapLimits {
                    max_frames: 1,
                    max_encoded_bytes: usize::MAX,
                },
            ),
            Err(ReplayError::FrameLimit)
        );
        let payload = SnapPayload::new(vec![frame(1, 1); 91]);
        assert_eq!(
            validate_payload(&payload, SnapLimits::default()),
            Err(ReplayError::FrameLimit)
        );
    }

    #[test]
    fn byte_allowance_is_aggregate_inclusive_and_precedes_decoding() {
        let payload = SnapPayload::new(vec![frame(1, 1); 2]);
        let size = payload.frames[0].png.len() * 2;
        let limits = SnapLimits {
            max_frames: 2,
            max_encoded_bytes: size,
        };
        assert!(validate_payload(&payload, limits).is_ok());
        assert_eq!(
            validate_payload(
                &payload,
                SnapLimits {
                    max_encoded_bytes: size - 1,
                    ..limits
                },
            ),
            Err(ReplayError::ByteLimit)
        );
        let mut invalid = payload;
        invalid.frames[0].png = "?".into();
        assert_eq!(
            validate_payload(
                &invalid,
                SnapLimits {
                    max_encoded_bytes: 0,
                    ..limits
                },
            ),
            Err(ReplayError::ByteLimit)
        );
    }

    #[test]
    fn zero_oversized_and_metadata_mismatched_geometry_is_rejected() {
        for (width, height) in [
            (0, 1),
            (1, 0),
            (FRAME_WIDTH + 1, 1),
            (1, MAX_FRAME_HEIGHT + 1),
        ] {
            let mut bad = frame(1, 1);
            bad.width = width;
            bad.height = height;
            assert_eq!(validate(bad), Err(ReplayError::Geometry));
        }
        let mut bad = frame(1, 1);
        bad.width = 2;
        assert_eq!(validate(bad), Err(ReplayError::Png));
    }

    #[test]
    fn invalid_base64_is_not_forwarded_as_an_image() {
        for data in ["?", "not base64!", "🦀"] {
            let mut bad = frame(1, 1);
            bad.png = data.into();
            assert_eq!(validate(bad), Err(ReplayError::Encoding));
        }
    }

    #[test]
    fn every_truncated_png_prefix_and_trailing_data_is_rejected() {
        let original = frame(2, 2);
        let png = bytes(&original);
        for length in 0..png.len() {
            assert_eq!(
                validate(with_bytes(&original, &png[..length])),
                Err(ReplayError::Png)
            );
        }
        let mut trailing = png;
        trailing.extend_from_slice(b"trailing data");
        assert_eq!(
            validate(with_bytes(&original, &trailing)),
            Err(ReplayError::Png)
        );
    }

    #[test]
    fn ihdr_idat_and_iend_crc_corruption_is_rejected() {
        let original = frame(2, 2);
        let png = bytes(&original);
        let idat_end = 41 + read_u32(&png, 33).unwrap() as usize;
        for offset in [29, idat_end, png.len() - 1] {
            let mut corrupt = png.clone();
            corrupt[offset] ^= 1;
            assert_eq!(
                validate(with_bytes(&original, &corrupt)),
                Err(ReplayError::Png)
            );
        }
    }

    #[test]
    fn correct_png_crc_does_not_authorize_invalid_or_truncated_zlib() {
        let original = frame(2, 2);
        let png = bytes(&original);
        let idat = native_png_idat(&png, 2, 2).unwrap();
        assert_eq!(
            validate(with_idat(&original, b"not a zlib stream")),
            Err(ReplayError::Png)
        );
        assert_eq!(
            validate(with_idat(&original, &idat[..idat.len() - 1])),
            Err(ReplayError::Png)
        );
    }

    #[test]
    fn trailing_compressed_stream_and_wrong_pixel_count_are_rejected() {
        let original = frame(1, 1);
        let mut doubled = compress(&[0, 42, 42, 42]);
        doubled.extend_from_slice(&compress(&[]));
        assert_eq!(
            validate(with_idat(&original, &doubled)),
            Err(ReplayError::Png)
        );
        for raw in [vec![0; 3], vec![0; 5], vec![0; 1024 * 1024]] {
            assert_eq!(
                validate(with_idat(&original, &compress(&raw))),
                Err(ReplayError::Png)
            );
        }
    }

    #[test]
    fn png_scanline_filters_are_checked_at_row_boundaries() {
        let original = frame(2, 2);
        let mut raw = vec![0u8; 14];
        raw[7] = 5;
        assert_eq!(
            validate(with_idat(&original, &compress(&raw))),
            Err(ReplayError::Png)
        );
        raw[7] = 4;
        assert!(validate(with_idat(&original, &compress(&raw))).is_ok());
    }

    #[test]
    fn unknown_json_fields_are_ignored_without_changing_the_payload() {
        let payload = SnapPayload::new(vec![frame(1, 1)]);
        let mut value = serde_json::to_value(&payload).unwrap();
        value["unrelated"] = json!("x".repeat(1024 * 1024));
        value["frames"][0]["unrelated"] = json!({"future":true});
        assert_eq!(parse_payload(&value, SnapLimits::default()), Ok(payload));
    }

    #[test]
    fn a_bad_final_frame_rejects_the_entire_archive() {
        let mut payload = SnapPayload::new(vec![frame(1, 1); 2]);
        payload.frames[1].png = "bad!".into();
        let details = json!({"snapcompact": payload});
        assert!(frames_from_details(Some(&details)).is_none());
    }

    #[test]
    fn invalid_direct_payload_does_not_modify_text_timestamp_or_existing_images() {
        let base = Message::User(UserMessage {
            content: UserContent::Blocks(vec![
                ContentBlock::Text(TextContent::new("complete text summary")),
                ContentBlock::Image(ImageContent {
                    data: "original image".into(),
                    mime_type: "image/png".into(),
                }),
            ]),
            timestamp: 42,
        });
        let mut payload = SnapPayload::new(vec![frame(1, 1)]);
        payload.frames[0].png = "invalid!".into();
        assert_eq!(
            serde_json::to_value(attach_frames(base.clone(), Some(&payload))).unwrap(),
            serde_json::to_value(base).unwrap()
        );
    }

    #[test]
    fn rejecting_new_payload_removes_stale_archive_but_keeps_other_details() {
        let mut bad = SnapPayload::new(vec![frame(1, 1)]);
        bad.schema = "foreign.v2".into();
        let details = payload_to_details(
            Some(json!({"snapcompact":{"old":true},"mode":"snapcompact","readFiles":["a"]})),
            &bad,
        );
        assert!(details.get(SNAPCOMPACT_DETAILS_KEY).is_none());
        assert_eq!(details["readFiles"], json!(["a"]));
        assert_eq!(details["mode"], "snapcompact");
    }

    #[test]
    fn attachment_is_idempotent_without_collapsing_repeated_history_frames() {
        let payload = SnapPayload::new(vec![frame(1, 1); 2]);
        let once = attach_frames(summary(), Some(&payload));
        let twice = attach_frames(once.clone(), Some(&payload));
        assert_eq!(
            serde_json::to_value(&once).unwrap(),
            serde_json::to_value(twice).unwrap()
        );
        let Message::User(user) = once else {
            panic!("expected summary");
        };
        let UserContent::Blocks(blocks) = user.content else {
            panic!("expected frames");
        };
        assert_eq!(
            blocks.len(),
            3,
            "two equal frames may represent distinct repeated history"
        );
        assert_eq!(user.timestamp, 42);
    }
}
