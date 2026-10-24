//! Bounded image envelope inspection. Never decompress guest image data.
//!
//! These checks authenticate the declared format and bound canvas dimensions,
//! not raster validity, animation frame counts, or downstream decoder memory.

use super::{invalid, limit};
use crate::error::Result;
use base64::Engine as _;

pub(super) fn dimensions(encoded: &str, mime: &str) -> Result<(u32, u32)> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| invalid("image must use canonical standard base64"))?;
    let (width, height) = match mime {
        "image/jpeg" => jpeg(&bytes)?,
        "image/gif" => gif(&bytes)?,
        "image/webp" => webp(&bytes)?,
        _ => return Err(invalid("unsupported image MIME type")),
    };
    if width == 0
        || height == 0
        || width > 16_384
        || height > 16_384
        || u64::from(width) * u64::from(height) > 16_777_216
    {
        return Err(limit());
    }
    Ok((width, height))
}

fn jpeg(bytes: &[u8]) -> Result<(u32, u32)> {
    if bytes.len() < 4 || !bytes.starts_with(b"\xff\xd8") || !bytes.ends_with(b"\xff\xd9") {
        return Err(invalid("JPEG start or end marker is missing"));
    }
    let end = bytes.len() - 2;
    let mut offset = 2;
    let mut size = None;
    while offset < end {
        if bytes[offset] != 0xff {
            return Err(invalid("JPEG marker expected"));
        }
        while offset < end && bytes[offset] == 0xff {
            offset += 1;
        }
        if offset >= end {
            return Err(invalid("truncated JPEG marker"));
        }
        let marker = bytes[offset];
        offset += 1;
        if marker == 0x01 {
            continue;
        }
        if marker == 0 || (0xd0..=0xd9).contains(&marker) || end - offset < 2 {
            return Err(invalid("invalid JPEG segment marker"));
        }
        let length = usize::from(u16::from_be_bytes([bytes[offset], bytes[offset + 1]]));
        if length < 2 || length > end - offset {
            return Err(invalid("truncated JPEG segment"));
        }
        let segment = &bytes[offset..offset + length];
        if matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf) {
            if length < 8
                || !(1..=4).contains(&segment[7])
                || length != 8 + 3 * usize::from(segment[7])
                || size.is_some()
            {
                return Err(invalid("invalid JPEG frame header"));
            }
            size = Some((
                u32::from(u16::from_be_bytes([segment[5], segment[6]])),
                u32::from(u16::from_be_bytes([segment[3], segment[4]])),
            ));
        }
        if marker == 0xda {
            if length < 6
                || !(1..=4).contains(&segment[2])
                || length != 6 + 2 * usize::from(segment[2])
            {
                return Err(invalid("invalid JPEG scan header"));
            }
            return size.ok_or_else(|| invalid("JPEG scan precedes its frame header"));
        }
        offset += length;
    }
    Err(invalid("JPEG frame or scan header is missing"))
}

fn gif(bytes: &[u8]) -> Result<(u32, u32)> {
    if bytes.len() < 14
        || !(bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"))
        || bytes.last() != Some(&0x3b)
    {
        return Err(invalid("GIF header or trailer is missing"));
    }
    if bytes[10] & 0x80 != 0 {
        let table_bytes = 3usize << (usize::from(bytes[10] & 7) + 1);
        if bytes.len() < 14 + table_bytes {
            return Err(invalid("truncated GIF color table"));
        }
    }
    Ok((
        u32::from(u16::from_le_bytes([bytes[6], bytes[7]])),
        u32::from(u16::from_le_bytes([bytes[8], bytes[9]])),
    ))
}

fn u24(bytes: &[u8]) -> u32 {
    u32::from(bytes[0]) | (u32::from(bytes[1]) << 8) | (u32::from(bytes[2]) << 16)
}

fn webp(bytes: &[u8]) -> Result<(u32, u32)> {
    if bytes.len() < 20 || !bytes.starts_with(b"RIFF") || &bytes[8..12] != b"WEBP" {
        return Err(invalid("WebP RIFF header is missing"));
    }
    let declared = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    if u64::from(declared) + 8 != bytes.len() as u64 {
        return Err(invalid("WebP RIFF length does not match the image"));
    }
    // Inspect every chunk boundary, including its mandatory odd-size padding.
    // Extended WebP uses its first VP8X chunk for the animation canvas size.
    let mut offset = 12;
    let mut size = None;
    while offset < bytes.len() {
        if bytes.len() - offset < 8 {
            return Err(invalid("truncated WebP chunk header"));
        }
        let chunk = &bytes[offset..offset + 4];
        let length = u32::from_le_bytes([
            bytes[offset + 4],
            bytes[offset + 5],
            bytes[offset + 6],
            bytes[offset + 7],
        ]);
        let length = usize::try_from(length).map_err(|_| limit())?;
        let start = offset + 8;
        if length > bytes.len() - start {
            return Err(invalid("truncated WebP chunk"));
        }
        let payload = &bytes[start..start + length];
        if offset == 12 {
            size = Some(match chunk {
                b"VP8X" if length == 10 => {
                    if payload[0] & 0xc1 != 0 || payload[1..4] != [0, 0, 0] {
                        return Err(invalid("invalid WebP extended header"));
                    }
                    (u24(&payload[4..7]) + 1, u24(&payload[7..10]) + 1)
                }
                b"VP8L" if length >= 5 && payload[0] == 0x2f && payload[4] >> 5 == 0 => {
                    let packed =
                        u32::from_le_bytes([payload[1], payload[2], payload[3], payload[4]]);
                    ((packed & 0x3fff) + 1, ((packed >> 14) & 0x3fff) + 1)
                }
                b"VP8 "
                    if length >= 10
                        && payload[0] & 1 == 0
                        && payload[3..6] == [0x9d, 0x01, 0x2a] =>
                {
                    (
                        u32::from(u16::from_le_bytes([payload[6], payload[7]]) & 0x3fff),
                        u32::from(u16::from_le_bytes([payload[8], payload[9]]) & 0x3fff),
                    )
                }
                _ => return Err(invalid("unsupported or malformed WebP image header")),
            });
        }
        offset = start + length;
        if length & 1 != 0 {
            if bytes.get(offset) != Some(&0) {
                return Err(invalid("WebP chunk padding is missing"));
            }
            offset += 1;
        }
    }
    size.ok_or_else(|| invalid("WebP image header is missing"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ContentBlock;
    use serde_json::json;

    fn encoded(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn jpeg_frame(width: u16, height: u16, marker: u8) -> Vec<u8> {
        let mut bytes = vec![0xff, 0xd8, 0xff, 0xe0, 0, 4, 0, 0, 0xff, marker, 0, 11, 8];
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&[
            1, 1, 0x11, 0, 0xff, 0xda, 0, 8, 1, 1, 0, 0, 63, 0, 0, 0xff, 0xd9,
        ]);
        bytes
    }

    fn gif_frame(width: u16, height: u16) -> Vec<u8> {
        let mut bytes = b"GIF89a".to_vec();
        bytes.extend_from_slice(&width.to_le_bytes());
        bytes.extend_from_slice(&height.to_le_bytes());
        bytes.extend_from_slice(&[0, 0, 0, 0x3b]);
        bytes
    }

    fn webp_chunk(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut bytes = b"RIFF".to_vec();
        let length = u32::try_from(payload.len()).unwrap();
        bytes.extend_from_slice(&(12 + length + (length & 1)).to_le_bytes());
        bytes.extend_from_slice(b"WEBP");
        bytes.extend_from_slice(kind);
        bytes.extend_from_slice(&length.to_le_bytes());
        bytes.extend_from_slice(payload);
        if length & 1 != 0 {
            bytes.push(0);
        }
        bytes
    }

    fn fixtures() -> Vec<(&'static str, Vec<u8>)> {
        vec![
            ("image/jpeg", jpeg_frame(2, 3, 0xc0)),
            ("image/jpeg", jpeg_frame(2, 3, 0xc2)),
            ("image/gif", gif_frame(2, 3)),
            (
                "image/webp",
                webp_chunk(b"VP8 ", &[0, 0, 0, 0x9d, 1, 0x2a, 2, 0, 3, 0]),
            ),
            ("image/webp", webp_chunk(b"VP8L", &[0x2f, 1, 0x80, 0, 0])),
            (
                "image/webp",
                webp_chunk(b"VP8X", &[0, 0, 0, 0, 1, 0, 0, 2, 0, 0]),
            ),
        ]
    }

    #[test]
    fn image_formats_produce_native_blocks_with_dimensions_without_data_duplication() {
        for (mime, bytes) in fixtures() {
            let data = encoded(&bytes);
            assert_eq!(dimensions(&data, mime).unwrap(), (2, 3));
            let frames =
                super::super::collect(&json!({"displays":[{"mimeType":mime,"data":data}]}))
                    .unwrap();
            assert!(matches!(&frames.content[0], ContentBlock::Image(image)
                if image.mime_type == mime && image.data == data));
            assert_eq!(frames.descriptors[0]["width"], 2);
            assert_eq!(frames.descriptors[0]["height"], 3);
            assert!(!frames.descriptors[0].to_string().contains(&data));
        }
    }

    #[test]
    fn truncated_images_and_mismatched_mime_types_are_rejected() {
        for (mime, bytes) in fixtures() {
            for end in 0..bytes.len() {
                assert!(
                    dimensions(&encoded(&bytes[..end]), mime).is_err(),
                    "{mime} at {end}"
                );
            }
            for other in ["image/jpeg", "image/gif", "image/webp"] {
                if mime != other {
                    assert!(dimensions(&encoded(&bytes), other).is_err());
                }
            }
            let data = encoded(&bytes);
            assert!(dimensions(&(data + "\n"), mime).is_err());
        }
    }

    #[test]
    fn dimension_limits_match_png_and_reject_zero_sized_canvases() {
        for (width, height) in [(0, 1), (1, 0), (16_385, 1), (4096, 4097)] {
            assert!(dimensions(&encoded(&jpeg_frame(width, height, 0xc0)), "image/jpeg").is_err());
            assert!(dimensions(&encoded(&gif_frame(width, height)), "image/gif").is_err());
        }
        assert_eq!(
            dimensions(&encoded(&jpeg_frame(4096, 4096, 0xc2)), "image/jpeg").unwrap(),
            (4096, 4096)
        );
        let huge = webp_chunk(b"VP8X", &[0, 0, 0, 0, 0xff, 0xff, 0xff, 0, 0, 0]);
        assert!(dimensions(&encoded(&huge), "image/webp").is_err());
    }

    #[test]
    fn segment_lengths_color_tables_and_riff_sizes_are_validated() {
        let mut jpeg = jpeg_frame(2, 3, 0xc0);
        jpeg[4..6].copy_from_slice(&[0, 0]);
        assert!(dimensions(&encoded(&jpeg), "image/jpeg").is_err());
        let mut gif = gif_frame(2, 3);
        gif[10] = 0x80;
        assert!(dimensions(&encoded(&gif), "image/gif").is_err());
        let mut webp = webp_chunk(b"VP8L", &[0x2f, 1, 0x80, 0, 0]);
        *webp.last_mut().unwrap() = 1;
        assert!(dimensions(&encoded(&webp), "image/webp").is_err());
        webp[4] = 0;
        assert!(dimensions(&encoded(&webp), "image/webp").is_err());
    }

    #[test]
    fn malformed_image_discards_the_entire_collection() {
        let response = json!({"displays":[
            {"mimeType":"text/plain","data":"valid prefix"},
            {"mimeType":"image/jpeg","data":encoded(b"not a JPEG")},
        ]});
        assert!(super::super::collect(&response).is_err());
    }
}
