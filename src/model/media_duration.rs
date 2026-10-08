//! Bounded container metadata inspection for media context accounting.
//!
//! Read only the base64 groups covering container headers. Large `mdat`, WAVE
//! data, and WebM cluster bodies are skipped by checked offsets, so inspecting
//! a recording never allocates or decodes its entire payload. This is a best
//! effort duration hint, not a replacement for the provider's media decoder.
//!
//! Layout references: Apple's QuickTime movie-header atom specification,
//! RFC 8794 (EBML), Matroska's Info/Duration/TimestampScale elements,
//! Microsoft's WAVEFORMATEX, RFC 9639 section 8.2 (FLAC STREAMINFO), and
//! ID3v2.4 / MPEG layer III's Xing/Info frame-count metadata. Ogg uses RFC
//! 3533, RFC 7845 sections 3–5, RFC 6716 section 3.1, and the Xiph Vorbis I
//! specification's identification header and Appendix A encapsulation rules.

use base64::Engine as _;
use std::time::Duration;

/// Bound total metadata work independently of the recording's encoded size.
const MAX_HEADER_BYTES: usize = 128 * 1024;

struct EncodedMedia<'a> {
    data: &'a [u8],
    len: usize,
    remaining: usize,
}

impl<'a> EncodedMedia<'a> {
    fn new(data: &'a str) -> Option<Self> {
        // Whitespace is not canonical base64. Do not scan an arbitrarily
        // long malformed suffix before the metadata-work budget starts.
        let data = data.as_bytes();
        let padding = data
            .iter()
            .rev()
            .take_while(|byte| **byte == b'=')
            .take(3)
            .count();
        if data.is_empty()
            || padding > 2
            || data.len() % 4 == 1
            || (padding > 0 && !data.len().is_multiple_of(4))
        {
            return None;
        }
        let len = (data.len() / 4)
            .checked_mul(3)?
            .checked_add(data.len() % 4 * 3 / 4)?
            .checked_sub(padding)?;
        let mut reader = Self {
            data,
            len,
            remaining: MAX_HEADER_BYTES,
        };
        // Check the final group too, including its padding and unused bits.
        reader.read::<1>(len.checked_sub(1)?)?;
        Some(reader)
    }

    fn read<const N: usize>(&mut self, offset: usize) -> Option<[u8; N]> {
        if N > 64 || offset.checked_add(N)? > self.len {
            return None;
        }
        let start = (offset / 3).checked_mul(4)?;
        let end = offset
            .checked_add(N)?
            .div_ceil(3)
            .checked_mul(4)?
            .min(self.data.len());
        let encoded = self.data.get(start..end)?;
        self.remaining = self.remaining.checked_sub(encoded.len())?;
        // An unaligned 64-byte read covers at most 66 decoded bytes. No
        // allocation depends on an untrusted container field or payload size.
        let mut decoded = [0_u8; 66];
        let decoded_len = base64::engine::general_purpose::STANDARD
            .decode_slice(encoded, &mut decoded)
            .or_else(|_| {
                base64::engine::general_purpose::STANDARD_NO_PAD.decode_slice(encoded, &mut decoded)
            })
            .ok()?;
        let mut result = [0_u8; N];
        result.copy_from_slice(
            decoded
                .get(..decoded_len)?
                .get(offset % 3..offset % 3 + N)?,
        );
        Some(result)
    }

    fn be_u32(&mut self, offset: usize) -> Option<u32> {
        self.read(offset).map(u32::from_be_bytes)
    }

    fn be_u64(&mut self, offset: usize) -> Option<u64> {
        self.read(offset).map(u64::from_be_bytes)
    }

    fn le_u32(&mut self, offset: usize) -> Option<u32> {
        self.read(offset).map(u32::from_le_bytes)
    }
}

pub(super) fn inspect(data: &str, mime_type: &str) -> Option<Duration> {
    let mut reader = EncodedMedia::new(data)?;
    let mime = mime_type.split(';').next()?.trim().to_ascii_lowercase();
    match mime.as_str() {
        "video/mp4" | "video/mov" | "video/quicktime" | "audio/mp4" | "audio/m4a"
        | "audio/x-m4a" => iso_duration(&mut reader),
        "video/webm" | "audio/webm" | "video/x-matroska" | "audio/x-matroska" => {
            webm_duration(&mut reader)
        }
        "audio/wav" | "audio/wave" | "audio/x-wav" => wave_duration(&mut reader),
        "audio/flac" | "audio/x-flac" => flac_duration(&mut reader),
        "audio/mpeg" | "audio/mp3" => mp3_duration(&mut reader),
        "audio/ogg" | "audio/x-ogg" | "audio/opus" | "audio/vorbis" | "application/ogg" => {
            ogg_duration(&mut reader)
        }
        _ => None,
    }
}

fn duration_from_units(units: u64, units_per_second: u64) -> Option<Duration> {
    if units == 0 || units_per_second == 0 {
        return None;
    }
    let nanos = (u128::from(units) * 1_000_000_000).div_ceil(u128::from(units_per_second));
    Some(Duration::new(
        u64::try_from(nanos / 1_000_000_000).ok()?,
        u32::try_from(nanos % 1_000_000_000).ok()?,
    ))
}

struct IsoBox {
    kind: [u8; 4],
    body: usize,
    end: usize,
}

fn iso_box(reader: &mut EncodedMedia<'_>, offset: usize, limit: usize) -> Option<IsoBox> {
    if offset.checked_add(8)? > limit {
        return None;
    }
    let short_size = reader.be_u32(offset)?;
    let kind = reader.read(offset + 4)?;
    let (size, header) = match short_size {
        0 => (limit.checked_sub(offset)?, 8),
        1 => {
            if offset.checked_add(16)? > limit {
                return None;
            }
            (usize::try_from(reader.be_u64(offset + 8)?).ok()?, 16)
        }
        size => (usize::try_from(size).ok()?, 8),
    };
    let end = offset.checked_add(size)?;
    if size < header || end > limit || end > reader.len {
        return None;
    }
    Some(IsoBox {
        kind,
        body: offset + header,
        end,
    })
}

fn movie_header_duration(reader: &mut EncodedMedia<'_>, header: &IsoBox) -> Option<Duration> {
    let version = reader.read::<1>(header.body)?[0];
    let (scale_offset, duration_offset, duration_size) = match version {
        0 => (12, 16, 4),
        1 => (20, 24, 8),
        _ => return None,
    };
    if header.body.checked_add(duration_offset + duration_size)? > header.end {
        return None;
    }
    let scale = u64::from(reader.be_u32(header.body + scale_offset)?);
    let units = if version == 0 {
        let units = reader.be_u32(header.body + duration_offset)?;
        if units == u32::MAX {
            return None;
        }
        u64::from(units)
    } else {
        let units = reader.be_u64(header.body + duration_offset)?;
        if units == u64::MAX {
            return None;
        }
        units
    };
    duration_from_units(units, scale)
}

fn iso_duration(reader: &mut EncodedMedia<'_>) -> Option<Duration> {
    let mut offset = 0;
    let mut duration = None;
    let end = reader.len;
    while offset < end {
        let outer = iso_box(reader, offset, end)?;
        if &outer.kind == b"moov" {
            let mut child = outer.body;
            while child < outer.end {
                let header = iso_box(reader, child, outer.end)?;
                if &header.kind == b"mvhd" {
                    if duration.is_some() {
                        return None;
                    }
                    duration = Some(movie_header_duration(reader, &header)?);
                }
                child = header.end;
            }
        }
        offset = outer.end;
    }
    duration
}

fn wave_duration(reader: &mut EncodedMedia<'_>) -> Option<Duration> {
    let header = reader.read::<12>(0)?;
    if &header[..4] != b"RIFF" || &header[8..] != b"WAVE" {
        return None;
    }
    let end = usize::try_from(u32::from_le_bytes(header[4..8].try_into().ok()?))
        .ok()?
        .checked_add(8)?;
    if end < 12 || end > reader.len {
        return None;
    }
    let mut offset = 12;
    let mut format = None;
    let mut data_bytes = 0_u64;
    while offset < end {
        if offset.checked_add(8)? > end {
            return None;
        }
        let kind = reader.read::<4>(offset)?;
        let size = usize::try_from(reader.le_u32(offset + 4)?).ok()?;
        let body = offset + 8;
        let body_end = body.checked_add(size)?;
        if body_end > end {
            return None;
        }
        match &kind {
            b"fmt " => {
                if format.is_some() || size < 16 {
                    return None;
                }
                let bytes = reader.read::<16>(body)?;
                let tag = u16::from_le_bytes(bytes[..2].try_into().ok()?);
                let channels = u64::from(u16::from_le_bytes(bytes[2..4].try_into().ok()?));
                let rate = u64::from(u32::from_le_bytes(bytes[4..8].try_into().ok()?));
                let byte_rate = u64::from(u32::from_le_bytes(bytes[8..12].try_into().ok()?));
                let align = u64::from(u16::from_le_bytes(bytes[12..14].try_into().ok()?));
                let bits = u64::from(u16::from_le_bytes(bytes[14..16].try_into().ok()?));
                // For compressed WAVE, average byte rate is not a duration.
                // Only fixed-width PCM and IEEE float samples are counted.
                if !matches!(tag, 1 | 3)
                    || channels == 0
                    || rate == 0
                    || align == 0
                    || bits == 0
                    || !bits.is_multiple_of(8)
                    || channels.checked_mul(bits / 8)? != align
                    || rate.checked_mul(align)? != byte_rate
                {
                    return None;
                }
                format = Some((rate, align));
            }
            b"data" => data_bytes = data_bytes.checked_add(u64::try_from(size).ok()?)?,
            _ => {}
        }
        offset = body_end.checked_add(size % 2)?;
        if offset > end {
            return None;
        }
    }
    let (rate, align) = format?;
    if !data_bytes.is_multiple_of(align) {
        return None;
    }
    duration_from_units(data_bytes / align, rate)
}

fn flac_duration(reader: &mut EncodedMedia<'_>) -> Option<Duration> {
    let header = reader.read::<8>(0)?;
    if &header[..4] != b"fLaC" || header[4] & 0x7f != 0 || header[5..] != [0, 0, 34] {
        return None;
    }
    // STREAMINFO is mandatory and first. Its sample rate (20 bits), channels
    // (3), sample width (5), and total samples (36) share eight bytes.
    if reader.len < 42 {
        return None;
    }
    let packed = reader.be_u64(18)?;
    let rate = packed >> 44;
    let samples = packed & ((1_u64 << 36) - 1);
    duration_from_units(samples, rate)
}

fn mp3_duration(reader: &mut EncodedMedia<'_>) -> Option<Duration> {
    let mut offset = 0_usize;
    if reader.read::<3>(0)? == *b"ID3" {
        let tag = reader.read::<10>(0)?;
        if !(2..=4).contains(&tag[3]) || tag[4] == 0xff {
            return None;
        }
        let mut tag_size = 0_usize;
        for byte in &tag[6..10] {
            if byte & 0x80 != 0 {
                return None;
            }
            tag_size = tag_size.checked_mul(128)?.checked_add(usize::from(*byte))?;
        }
        // The v2.4 footer, when present, is outside the synchsafe tag size.
        let footer = if tag[3] == 4 && tag[5] & 0x10 != 0 {
            10
        } else {
            0
        };
        offset = 10_usize.checked_add(tag_size)?.checked_add(footer)?;
    }
    let header = reader.be_u32(offset)?;
    let version = (header >> 19) & 3;
    let rate_index = usize::try_from((header >> 10) & 3).ok()?;
    let bitrate_index = usize::try_from((header >> 12) & 15).ok()?;
    if header >> 21 != 0x7ff
        || version == 1
        || (header >> 17) & 3 != 1
        || rate_index == 3
        || !(1..=14).contains(&bitrate_index)
        || header & (1 << 16) == 0
    {
        // CRC-protected or headerless streams retain the fallback estimate.
        return None;
    }
    let divisor = match version {
        3 => 1,
        2 => 2,
        _ => 4,
    };
    let rate = [44_100_u64, 48_000, 32_000][rate_index] / divisor;
    let samples_per_frame = if version == 3 { 1_152_u64 } else { 576 };
    let mono = (header >> 6) & 3 == 3;
    let side_info = match (version == 3, mono) {
        (true, false) => 32,
        (true, true) | (false, false) => 17,
        (false, true) => 9,
    };
    let bitrates = if version == 3 {
        [
            32_u64, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
        ]
    } else {
        [
            8_u64, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160,
        ]
    };
    let bitrate = bitrates[bitrate_index - 1];
    let frame_bytes = samples_per_frame * 125 * bitrate / rate + u64::from((header >> 9) & 1);
    let frame_end = offset.checked_add(usize::try_from(frame_bytes).ok()?)?;
    let info = offset.checked_add(4 + side_info)?;
    if info.checked_add(12)? > frame_end || frame_end > reader.len {
        return None;
    }
    let kind = reader.read::<4>(info)?;
    if !matches!(&kind, b"Xing" | b"Info") || reader.be_u32(info + 4)? & 1 == 0 {
        return None;
    }
    let frames = u64::from(reader.be_u32(info + 8)?);
    // Includes encoder padding conservatively; never infer duration from a
    // nominal bitrate, which would undercount variable-bitrate recordings.
    duration_from_units(frames.checked_mul(samples_per_frame)?, rate)
}

struct OggPage {
    flags: u8,
    granule: u64,
    serial: u32,
    sequence: u32,
    segments: [u8; 255],
    segment_count: usize,
    body: usize,
    end: usize,
}

fn ogg_page(reader: &mut EncodedMedia<'_>, offset: usize) -> Option<OggPage> {
    let header = reader.read::<27>(offset)?;
    if &header[..4] != b"OggS" || header[4] != 0 || header[5] & !7 != 0 {
        return None;
    }
    let segment_count = usize::from(header[26]);
    if segment_count == 0 {
        return None;
    }
    let table = offset.checked_add(27)?;
    let body = table.checked_add(segment_count)?;
    let mut segments = [0_u8; 255];
    // Decode small groups, not every lacing byte separately. The fixed table
    // and existing read budget bound work even for a page/packet flood.
    let mut copied = 0;
    while copied + 16 <= segment_count {
        segments[copied..copied + 16].copy_from_slice(&reader.read::<16>(table + copied)?);
        copied += 16;
    }
    for (index, segment) in segments[copied..segment_count].iter_mut().enumerate() {
        *segment = reader.read::<1>(table + copied + index)?[0];
    }
    let payload_bytes: usize = segments[..segment_count]
        .iter()
        .map(|segment| usize::from(*segment))
        .sum();
    let end = body.checked_add(payload_bytes)?;
    if end > reader.len {
        return None;
    }
    Some(OggPage {
        flags: header[5],
        granule: u64::from_le_bytes(header[6..14].try_into().ok()?),
        serial: u32::from_le_bytes(header[14..18].try_into().ok()?),
        sequence: u32::from_le_bytes(header[18..22].try_into().ok()?),
        segments,
        segment_count,
        body,
        end,
    })
}

struct OggCodec {
    rate: u64,
    pre_skip: u64,
    headers_left: u8,
    opus: bool,
}

fn ogg_codec(reader: &mut EncodedMedia<'_>, page: &OggPage) -> Option<OggCodec> {
    // Both codecs put the complete identification packet alone on page zero.
    if page.flags != 2 || page.sequence != 0 || page.granule != 0 || page.segment_count != 1 {
        return None;
    }
    let magic = reader.read::<8>(page.body)?;
    if &magic == b"OpusHead" && page.segments[0] == 19 {
        let header = reader.read::<19>(page.body)?;
        if header[8] > 15 || !(1..=2).contains(&header[9]) || header[18] != 0 {
            // Other channel mappings require multistream packet framing.
            return None;
        }
        return Some(OggCodec {
            // The input-rate field is informational: granules always use 48k.
            rate: 48_000,
            pre_skip: u64::from(u16::from_le_bytes(header[10..12].try_into().ok()?)),
            headers_left: 1,
            opus: true,
        });
    }
    if &magic[..7] != b"\x01vorbis" || page.segments[0] != 30 {
        return None;
    }
    let header = reader.read::<30>(page.body)?;
    let short = header[28] & 15;
    let long = header[28] >> 4;
    let rate = u64::from(u32::from_le_bytes(header[12..16].try_into().ok()?));
    if header[7..11] != [0; 4]
        || header[11] == 0
        || rate == 0
        || !(6..=13).contains(&short)
        || !(short..=13).contains(&long)
        || header[29] & 1 == 0
    {
        return None;
    }
    Some(OggCodec {
        rate,
        pre_skip: 0,
        headers_left: 2,
        opus: false,
    })
}

/// Only the TOC and optional frame count are needed to establish the initial
/// Opus granule origin; compressed frame data is neither read nor decoded.
fn opus_packet_samples(prefix: &[u8]) -> Option<u64> {
    let toc = *prefix.first()?;
    let frame_samples = if toc & 0x80 != 0 {
        120_u64 << ((toc >> 3) & 3)
    } else if toc & 0x60 == 0x60 {
        480_u64 << ((toc >> 3) & 1)
    } else {
        [480_u64, 960, 1_920, 2_880][usize::from((toc >> 3) & 3)]
    };
    let frames = match toc & 3 {
        0 => 1,
        1 | 2 => 2,
        _ => u64::from(*prefix.get(1)? & 0x3f),
    };
    let samples = frames * frame_samples;
    (samples > 0 && samples <= 5_760).then_some(samples)
}

#[allow(clippy::too_many_lines)]
fn ogg_duration(reader: &mut EncodedMedia<'_>) -> Option<Duration> {
    let first = ogg_page(reader, 0)?;
    let mut codec = ogg_codec(reader, &first)?;
    let mut offset = first.end;
    let mut sequence = 1_u32;
    let mut packet_len = 0_usize;
    let mut prefix = [0_u8; 8];
    let mut prefix_len = 0_usize;
    let mut first_audio = true;
    let mut initial_granule = 0_u64;
    let mut previous_granule = 0_u64;
    while offset < reader.len {
        let page = ogg_page(reader, offset)?;
        if page.serial != first.serial
            || page.sequence != sequence
            || page.flags & 2 != 0
            || (page.flags & 1 != 0) != (packet_len > 0)
        {
            return None;
        }
        let headers_on_page = codec.headers_left > 0;
        let mut completed = 0_usize;
        let mut initial_samples = 0_u64;
        let mut body = page.body;
        for (index, segment) in page.segments[..page.segment_count].iter().enumerate() {
            let size = usize::from(*segment);
            let wanted: usize = if codec.headers_left > 0 {
                8
            } else if codec.opus && first_audio {
                2
            } else {
                0
            };
            let take = size.min(wanted.saturating_sub(prefix_len));
            for byte in &mut prefix[prefix_len..prefix_len + take] {
                *byte = reader.read::<1>(body)?[0];
                body += 1;
            }
            prefix_len += take;
            body += size - take;
            packet_len = packet_len.checked_add(size)?;
            if *segment == 255 {
                continue;
            }
            if packet_len == 0 {
                return None;
            }
            completed += 1;
            if codec.headers_left > 0 {
                let signature: &[u8] = if codec.opus {
                    b"OpusTags"
                } else if codec.headers_left == 2 {
                    b"\x03vorbis"
                } else {
                    b"\x05vorbis"
                };
                let minimum_len = if codec.opus || codec.headers_left == 2 {
                    16
                } else {
                    8
                };
                if packet_len < minimum_len || !prefix[..prefix_len].starts_with(signature) {
                    return None;
                }
                codec.headers_left -= 1;
                if codec.headers_left == 0 && index + 1 != page.segment_count {
                    return None;
                }
            } else if codec.opus && first_audio {
                initial_samples =
                    initial_samples.checked_add(opus_packet_samples(&prefix[..prefix_len])?)?;
            }
            packet_len = 0;
            prefix_len = 0;
        }
        if headers_on_page {
            if page.granule != 0 && !(completed == 0 && page.granule == u64::MAX) {
                return None;
            }
        } else if completed == 0 {
            if page.granule != u64::MAX {
                return None;
            }
        } else {
            if page.granule > 0x7fff_ffff_ffff_ffff || page.granule < previous_granule {
                return None;
            }
            if first_audio {
                if codec.opus {
                    // RFC7845 §4.5 permits cropped streams with a nonzero
                    // origin. Only a first-and-final page may trim below the
                    // samples encoded on it, in which case its origin is zero.
                    initial_granule = if page.flags & 4 != 0 {
                        page.granule.saturating_sub(initial_samples)
                    } else {
                        page.granule.checked_sub(initial_samples)?
                    };
                } else if completed <= 2 {
                    // Xiph Appendix A flushes the second audio packet when
                    // the origin is shifted. Resolving that ambiguous layout
                    // requires codec setup; use the fallback, not a guess.
                    return None;
                }
                first_audio = false;
            }
            previous_granule = page.granule;
        }
        if page.flags & 4 != 0 {
            if headers_on_page || completed == 0 || packet_len != 0 || page.end != reader.len {
                return None;
            }
            return duration_from_units(
                page.granule
                    .checked_sub(initial_granule)?
                    .checked_sub(codec.pre_skip)?,
                codec.rate,
            );
        }
        offset = page.end;
        sequence = sequence.checked_add(1)?;
    }
    None
}

struct EbmlElement {
    id: u64,
    body: usize,
    end: usize,
    unknown_size: bool,
}

fn ebml_vint(reader: &mut EncodedMedia<'_>, offset: usize, id: bool) -> Option<(u64, usize)> {
    let first = reader.read::<1>(offset)?[0];
    let len = usize::try_from(first.leading_zeros())
        .ok()?
        .checked_add(1)?;
    if len > 8 || (id && len > 4) {
        return None;
    }
    let mut value = u64::from(if id {
        first
    } else {
        first & (0x7f >> (len - 1))
    });
    for index in 1..len {
        value = (value << 8) | u64::from(reader.read::<1>(offset.checked_add(index)?)?[0]);
    }
    Some((value, len))
}

fn ebml_element(reader: &mut EncodedMedia<'_>, offset: usize, limit: usize) -> Option<EbmlElement> {
    let (id, id_len) = ebml_vint(reader, offset, true)?;
    let (size, size_len) = ebml_vint(reader, offset.checked_add(id_len)?, false)?;
    let body = offset.checked_add(id_len)?.checked_add(size_len)?;
    let unknown_size = size == (1_u64 << (size_len * 7)) - 1;
    let end = if unknown_size {
        limit
    } else {
        body.checked_add(usize::try_from(size).ok()?)?
    };
    if body > limit || end > limit || end > reader.len {
        return None;
    }
    Some(EbmlElement {
        id,
        body,
        end,
        unknown_size,
    })
}

#[allow(clippy::cast_precision_loss)]
fn webm_info_duration(reader: &mut EncodedMedia<'_>, info: &EbmlElement) -> Option<Duration> {
    let mut offset = info.body;
    let mut scale = None;
    let mut ticks = None;
    while offset < info.end {
        let field = ebml_element(reader, offset, info.end)?;
        if field.unknown_size {
            return None;
        }
        match field.id {
            0x002a_d7b1 => {
                let len = field.end - field.body;
                if scale.is_some() || !(1..=8).contains(&len) {
                    return None;
                }
                let mut value = 0_u64;
                for index in 0..len {
                    value = (value << 8) | u64::from(reader.read::<1>(field.body + index)?[0]);
                }
                if value == 0 {
                    return None;
                }
                scale = Some(value);
            }
            0x4489 => {
                if ticks.is_some() {
                    return None;
                }
                ticks = Some(match field.end - field.body {
                    4 => f64::from(f32::from_be_bytes(reader.read(field.body)?)),
                    8 => f64::from_be_bytes(reader.read(field.body)?),
                    _ => return None,
                });
            }
            _ => {}
        }
        offset = field.end;
    }
    let seconds = ticks? * scale.unwrap_or(1_000_000) as f64 / 1_000_000_000.0;
    if !seconds.is_finite() || seconds <= 0.0 {
        return None;
    }
    Duration::try_from_secs_f64(seconds)
        .ok()
        .filter(|duration| !duration.is_zero())
}

fn webm_duration(reader: &mut EncodedMedia<'_>) -> Option<Duration> {
    let end = reader.len;
    let header = ebml_element(reader, 0, end)?;
    if header.id != 0x1a45_dfa3 || header.unknown_size {
        return None;
    }
    let mut offset = header.end;
    while offset < end {
        let segment = ebml_element(reader, offset, end)?;
        if segment.id == 0x1853_8067 {
            // Chained EBML documents need a whole-presentation duration; a
            // first-segment hint would silently omit the remaining segments.
            if segment.end != end {
                return None;
            }
            let mut child = segment.body;
            let mut duration = None;
            while child < segment.end {
                let field = ebml_element(reader, child, segment.end)?;
                // An unknown-size Segment ends at the next root/header, not
                // necessarily EOF. Inspect every child header before accepting
                // its duration; payload bodies still need no decoding.
                if field.unknown_size || matches!(field.id, 0x1a45_dfa3 | 0x1853_8067) {
                    return None;
                }
                if field.id == 0x1549_a966 {
                    if duration.is_some() {
                        return None;
                    }
                    duration = Some(webm_info_duration(reader, &field)?);
                }
                child = field.end;
            }
            return duration;
        }
        if segment.unknown_size || segment.id == 0x1a45_dfa3 {
            return None;
        }
        offset = segment.end;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut bytes = u32::try_from(body.len() + 8)
            .unwrap()
            .to_be_bytes()
            .to_vec();
        bytes.extend_from_slice(kind);
        bytes.extend_from_slice(body);
        bytes
    }

    fn movie(scale: u32, units: u64, version: u8) -> Vec<u8> {
        let mut header = vec![0; if version == 1 { 32 } else { 20 }];
        header[0] = version;
        if version == 1 {
            header[20..24].copy_from_slice(&scale.to_be_bytes());
            header[24..32].copy_from_slice(&units.to_be_bytes());
        } else {
            header[12..16].copy_from_slice(&scale.to_be_bytes());
            header[16..20].copy_from_slice(&u32::try_from(units).unwrap().to_be_bytes());
        }
        boxed(b"moov", &boxed(b"mvhd", &header))
    }

    #[test]
    fn random_access_reads_padded_and_unpadded_data_at_every_group_offset() {
        for len in 1..=150 {
            let bytes: Vec<u8> = (0..len).map(|value| u8::try_from(value).unwrap()).collect();
            for data in [
                encode(&bytes),
                base64::engine::general_purpose::STANDARD_NO_PAD.encode(&bytes),
            ] {
                let mut reader = EncodedMedia::new(&data).unwrap();
                assert_eq!(reader.len, bytes.len());
                for offset in 0..len {
                    assert_eq!(reader.read::<1>(offset), Some([bytes[offset]]));
                    if offset + 64 <= len {
                        assert_eq!(
                            reader
                                .read::<64>(offset)
                                .as_ref()
                                .map(|value| value.as_slice()),
                            Some(&bytes[offset..offset + 64])
                        );
                    }
                }
                assert!(reader.read::<1>(len).is_none());
                assert!(reader.read::<8>(usize::MAX).is_none());
            }
        }
    }

    #[test]
    fn movie_duration_uses_time_scale_and_skips_large_payload_before_moov() {
        for version in [0, 1] {
            let mut bytes = boxed(b"mdat", &vec![0; 2 * 1024 * 1024]);
            bytes.extend(movie(90_000, 2_745_000, version));
            let data = encode(&bytes);
            assert_eq!(
                inspect(&data, "video/mp4"),
                Some(Duration::from_millis(30_500))
            );
            let mut reader = EncodedMedia::new(&data).unwrap();
            assert_eq!(
                iso_duration(&mut reader),
                Some(Duration::from_millis(30_500))
            );
            assert!(MAX_HEADER_BYTES - reader.remaining < 256);
        }
        assert_eq!(
            inspect(&encode(&movie(48_000, 480_000, 1)), "audio/m4a"),
            Some(Duration::from_secs(10))
        );
        assert!(inspect(&encode(&movie(0, 100, 0)), "video/mp4").is_none());
        assert!(inspect(&encode(&movie(1, u64::MAX, 1)), "video/mp4").is_none());
    }

    #[test]
    fn movie_rejects_truncation_overflow_duplicate_duration_and_bad_child_bounds() {
        let valid = movie(1_000, 30_000, 0);
        for len in 0..valid.len() {
            assert!(inspect(&encode(&valid[..len]), "video/mp4").is_none());
        }
        let mut duplicate = valid.clone();
        duplicate.extend_from_slice(&valid);
        assert!(inspect(&encode(&duplicate), "video/mp4").is_none());
        let mut oversized = valid.clone();
        oversized[8..12].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(inspect(&encode(&oversized), "video/mp4").is_none());
        let mut extended = 1_u32.to_be_bytes().to_vec();
        extended.extend_from_slice(b"moov");
        extended.extend_from_slice(&u64::MAX.to_be_bytes());
        assert!(inspect(&encode(&extended), "video/mp4").is_none());
        let mut bad_version = valid;
        bad_version[16] = 2;
        assert!(inspect(&encode(&bad_version), "video/mp4").is_none());
    }

    fn wave(rate: u32, channels: u16, bits: u16, frames: u32) -> Vec<u8> {
        let align = channels * (bits / 8);
        let data_len = frames * u32::from(align);
        let mut bytes = b"RIFF".to_vec();
        bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&channels.to_le_bytes());
        bytes.extend_from_slice(&rate.to_le_bytes());
        bytes.extend_from_slice(&(rate * u32::from(align)).to_le_bytes());
        bytes.extend_from_slice(&align.to_le_bytes());
        bytes.extend_from_slice(&bits.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_len.to_le_bytes());
        bytes.resize(44 + usize::try_from(data_len).unwrap(), 0);
        bytes
    }

    #[test]
    fn wave_duration_counts_samples_instead_of_bitrate_or_channels() {
        for (rate, channels, bits) in [(8_000, 1, 8), (48_000, 2, 24)] {
            let bytes = wave(rate, channels, bits, rate * 3);
            assert_eq!(
                inspect(&encode(&bytes), "audio/wav"),
                Some(Duration::from_secs(3))
            );
        }
        let mut invalid = wave(8_000, 1, 16, 8_000);
        invalid[28..32].copy_from_slice(&1_u32.to_le_bytes());
        assert!(inspect(&encode(&invalid), "audio/wav").is_none());
        let mut compressed = wave(8_000, 1, 16, 8_000);
        compressed[20..22].copy_from_slice(&2_u16.to_le_bytes());
        assert!(inspect(&encode(&compressed), "audio/wav").is_none());
        let valid = wave(8_000, 1, 16, 8_000);
        assert!(inspect(&encode(&valid[..valid.len() - 1]), "audio/wav").is_none());
    }

    #[test]
    fn flac_duration_uses_streaminfo_samples_and_rejects_unknown_length() {
        let mut bytes = b"fLaC\x80\x00\x00\x22".to_vec();
        bytes.resize(42, 0);
        let packed = (48_000_u64 << 44) | (15_u64 << 36) | 2_880_000;
        bytes[18..26].copy_from_slice(&packed.to_be_bytes());
        assert_eq!(
            inspect(&encode(&bytes), "audio/flac"),
            Some(Duration::from_secs(60))
        );
        bytes[18..26].copy_from_slice(&(48_000_u64 << 44).to_be_bytes());
        assert!(inspect(&encode(&bytes), "audio/flac").is_none());
    }

    #[test]
    fn mp3_duration_uses_frame_count_and_skips_id3_metadata() {
        for kind in [b"Xing", b"Info"] {
            // MPEG1 layer III, 128 kbit/s, 48 kHz, stereo: 384-byte frame.
            let mut frame = vec![0; 384];
            frame[..4].copy_from_slice(&[0xff, 0xfb, 0x94, 0]);
            frame[36..40].copy_from_slice(kind);
            frame[40..44].copy_from_slice(&1_u32.to_be_bytes());
            frame[44..48].copy_from_slice(&2_500_u32.to_be_bytes());
            assert_eq!(
                inspect(&encode(&frame), "audio/mpeg"),
                Some(Duration::from_secs(60))
            );
            let mut tagged = b"ID3\x04\x00\x00\x00\x00\x00\x03abc".to_vec();
            tagged.extend_from_slice(&frame);
            assert_eq!(
                inspect(&encode(&tagged), "audio/mpeg"),
                Some(Duration::from_secs(60))
            );
            frame[44..48].fill(0);
            assert!(inspect(&encode(&frame), "audio/mpeg").is_none());
            tagged[6] = 0x80;
            assert!(inspect(&encode(&tagged), "audio/mpeg").is_none());
        }
    }

    fn ogg_fixture_page(
        sequence: u32,
        flags: u8,
        granule: u64,
        laces: &[u8],
        body: &[u8],
    ) -> Vec<u8> {
        assert_eq!(
            laces.iter().map(|lace| usize::from(*lace)).sum::<usize>(),
            body.len()
        );
        let mut bytes = b"OggS".to_vec();
        bytes.extend_from_slice(&[0, flags]);
        bytes.extend_from_slice(&granule.to_le_bytes());
        bytes.extend_from_slice(&42_u32.to_le_bytes());
        bytes.extend_from_slice(&sequence.to_le_bytes());
        // These are container metadata fixtures, not decoder/CRC fixtures.
        bytes.extend_from_slice(&[0; 4]);
        bytes.push(u8::try_from(laces.len()).unwrap());
        bytes.extend_from_slice(laces);
        bytes.extend_from_slice(body);
        bytes
    }

    fn opus_identification(pre_skip: u16) -> Vec<u8> {
        let mut packet = b"OpusHead\x01\x02".to_vec();
        packet.extend_from_slice(&pre_skip.to_le_bytes());
        // Informational source sample rate must not replace the 48 kHz clock.
        packet.extend_from_slice(&8_000_u32.to_le_bytes());
        packet.extend_from_slice(&[0, 0, 0]);
        ogg_fixture_page(0, 2, 0, &[19], &packet)
    }

    fn opus_headers() -> Vec<u8> {
        let mut bytes = opus_identification(312);
        bytes.extend(ogg_fixture_page(
            1,
            0,
            0,
            &[16],
            b"OpusTags\0\0\0\0\0\0\0\0",
        ));
        bytes
    }

    fn opus_audio_pages(sequence: u32, origin: u64) -> Vec<u8> {
        let mut bytes = Vec::new();
        for index in 0..6_u32 {
            // 250 packets of one 20ms CELT frame: five seconds per page.
            bytes.extend(ogg_fixture_page(
                sequence + index,
                0,
                origin + u64::from(index + 1) * 250 * 960,
                &[1; 250],
                &[0xf8; 250],
            ));
        }
        // Final granule trims encoder padding; pre-skip leaves exactly 30s.
        bytes.extend(ogg_fixture_page(
            sequence + 6,
            4,
            origin + 1_440_312,
            &[1],
            &[0xf8],
        ));
        bytes
    }

    fn vorbis_headers(rate: u32) -> Vec<u8> {
        let mut identification = b"\x01vorbis".to_vec();
        identification.extend_from_slice(&[0; 4]);
        identification.push(2);
        identification.extend_from_slice(&rate.to_le_bytes());
        identification.extend_from_slice(&[0; 12]);
        identification.extend_from_slice(&[0xb8, 1]);
        let mut bytes = ogg_fixture_page(0, 2, 0, &[30], &identification);
        let mut headers = b"\x03vorbis\0\0\0\0\0\0\0\0\x01".to_vec();
        headers.extend_from_slice(b"\x05vorbis\x01");
        bytes.extend(ogg_fixture_page(1, 0, 0, &[16, 8], &headers));
        bytes
    }

    #[test]
    fn ogg_duration_uses_codec_clock_preskip_and_initial_granule_origin() {
        for origin in [0, 96_000] {
            let mut bytes = opus_headers();
            bytes.extend(opus_audio_pages(2, origin));
            for mime in ["audio/ogg", "Audio/Opus; codecs=opus", "application/ogg"] {
                assert_eq!(
                    inspect(&encode(&bytes), mime),
                    Some(Duration::from_secs(30))
                );
            }
        }
        for rate in [24_000, 48_000] {
            let mut bytes = vorbis_headers(rate);
            bytes.extend(ogg_fixture_page(2, 0, u64::from(rate), &[1, 1, 1], &[0; 3]));
            bytes.extend(ogg_fixture_page(3, 4, u64::from(rate) * 2, &[1], &[0]));
            assert_eq!(
                inspect(&encode(&bytes), "audio/vorbis"),
                Some(Duration::from_secs(2))
            );
        }
        let mut trimmed = opus_headers();
        trimmed.extend(ogg_fixture_page(2, 4, 1_000, &[1, 1, 1], &[0xf8; 3]));
        assert_eq!(
            inspect(&encode(&trimmed), "audio/ogg"),
            Some(Duration::from_nanos(14_333_334))
        );
        let mut too_short = opus_headers();
        too_short.extend(ogg_fixture_page(2, 4, 311, &[1], &[0xf8]));
        assert!(inspect(&encode(&too_short), "audio/ogg").is_none());
        let mut invalid_initial = opus_headers();
        invalid_initial.extend(ogg_fixture_page(2, 0, 1_000, &[1, 1, 1], &[0xf8; 3]));
        invalid_initial.extend(ogg_fixture_page(3, 4, 3_000, &[1], &[0xf8]));
        assert!(inspect(&encode(&invalid_initial), "audio/ogg").is_none());
    }

    #[test]
    fn ogg_duration_skips_large_continued_comments_with_bounded_reads() {
        let mut bytes = opus_identification(312);
        let mut comment = vec![0; 65_026];
        comment[..8].copy_from_slice(b"OpusTags");
        // One long vendor string leaves an empty comment list at the end.
        comment[8..12].copy_from_slice(&65_010_u32.to_le_bytes());
        bytes.extend(ogg_fixture_page(
            1,
            0,
            u64::MAX,
            &[255; 255],
            &comment[..65_025],
        ));
        bytes.extend(ogg_fixture_page(2, 1, 0, &[1], &comment[65_025..]));
        bytes.extend(opus_audio_pages(3, 0));
        let data = encode(&bytes);
        let mut reader = EncodedMedia::new(&data).unwrap();
        assert_eq!(ogg_duration(&mut reader), Some(Duration::from_secs(30)));
        assert!(MAX_HEADER_BYTES - reader.remaining < 8_192);
        // A missing continuation flag cannot turn the tail into a new packet.
        bytes[opus_identification(312).len() + 27 + 255 + 65_025 + 5] = 0;
        assert!(inspect(&encode(&bytes), "audio/ogg").is_none());
    }

    #[test]
    fn ogg_rejects_truncated_chained_multiplexed_and_out_of_order_streams() {
        let headers = opus_headers();
        let first_audio = headers.len();
        let mut valid = headers.clone();
        valid.extend(opus_audio_pages(2, 0));
        for len in [0, 26, first_audio, valid.len() - 1, valid.len() - 29] {
            assert!(inspect(&encode(&valid[..len]), "audio/ogg").is_none());
        }
        let mut chained = valid.clone();
        chained.extend_from_slice(&valid);
        assert!(inspect(&encode(&chained), "audio/ogg").is_none());
        for (offset, replacement) in [
            (first_audio + 4, 1),   // Unknown Ogg version.
            (first_audio + 5, 2),   // Unexpected second BOS.
            (first_audio + 5, 8),   // Reserved header flags.
            (first_audio + 14, 43), // Another logical stream.
            (first_audio + 18, 3),  // Missing page sequence.
        ] {
            let mut malformed = valid.clone();
            malformed[offset] = replacement;
            assert!(inspect(&encode(&malformed), "audio/ogg").is_none());
        }
        let mut unfinished = valid.clone();
        let final_page = unfinished.len() - 29;
        unfinished[final_page + 5] = 0;
        assert!(inspect(&encode(&unfinished), "audio/ogg").is_none());
        let mut backwards = valid;
        backwards[final_page + 6..final_page + 14].copy_from_slice(&1_u64.to_le_bytes());
        assert!(inspect(&encode(&backwards), "audio/ogg").is_none());
        let mut pending = headers;
        pending.extend(ogg_fixture_page(2, 4, 48_312, &[1, 255], &[0xf8; 256]));
        assert!(inspect(&encode(&pending), "audio/ogg").is_none());
    }

    #[test]
    fn ogg_rejects_ambiguous_origins_bad_headers_and_excessive_metadata() {
        let mut ambiguous = vorbis_headers(48_000);
        ambiguous.extend(ogg_fixture_page(2, 0, 48_000, &[1, 1], &[0; 2]));
        ambiguous.extend(ogg_fixture_page(3, 4, 96_000, &[1], &[0]));
        assert!(inspect(&encode(&ambiguous), "audio/ogg").is_none());
        for (offset, replacement) in [(28 + 8, 16), (28 + 9, 0), (28 + 18, 1)] {
            let mut unsupported = opus_headers();
            unsupported.extend(opus_audio_pages(2, 0));
            unsupported[offset] = replacement;
            assert!(inspect(&encode(&unsupported), "audio/ogg").is_none());
        }
        for packet in [&b"\x83"[..], &b"\x83\0"[..], &b"\xfb\x3f"[..]] {
            let mut malformed = opus_headers();
            malformed.extend(ogg_fixture_page(
                2,
                4,
                1_000,
                &[u8::try_from(packet.len()).unwrap()],
                packet,
            ));
            assert!(inspect(&encode(&malformed), "audio/ogg").is_none());
        }
        let mut missing_header = opus_identification(312);
        missing_header.extend(ogg_fixture_page(1, 0, 0, &[8], b"OpusTags"));
        missing_header.extend(opus_audio_pages(2, 0));
        assert!(inspect(&encode(&missing_header), "audio/ogg").is_none());
        let mut flood = opus_headers();
        for sequence in 2..4_002 {
            flood.extend(ogg_fixture_page(
                sequence,
                0,
                u64::from(sequence - 1) * 960,
                &[1],
                &[0xf8],
            ));
        }
        flood.extend(ogg_fixture_page(4_002, 4, 4_000 * 960 + 312, &[1], &[0xf8]));
        assert!(inspect(&encode(&flood), "audio/ogg").is_none());
    }

    fn webm(ticks: f64, scale: Option<u32>, unknown_segment: bool) -> Vec<u8> {
        let mut info = vec![0x44, 0x89, 0x88];
        info.extend_from_slice(&ticks.to_be_bytes());
        if let Some(scale) = scale {
            info.extend_from_slice(&[0x2a, 0xd7, 0xb1, 0x84]);
            info.extend_from_slice(&scale.to_be_bytes());
        }
        let mut bytes = vec![0x1a, 0x45, 0xdf, 0xa3, 0x80, 0x18, 0x53, 0x80, 0x67];
        bytes.push(if unknown_segment {
            0xff
        } else {
            0x80 | u8::try_from(info.len() + 5).unwrap()
        });
        bytes.extend_from_slice(&[
            0x15,
            0x49,
            0xa9,
            0x66,
            0x80 | u8::try_from(info.len()).unwrap(),
        ]);
        bytes.extend(info);
        bytes
    }

    #[test]
    fn webm_duration_handles_default_scale_later_scale_and_unknown_segment_size() {
        for unknown in [false, true] {
            assert_eq!(
                inspect(&encode(&webm(30_500.0, None, unknown)), "video/webm"),
                Some(Duration::from_millis(30_500))
            );
            assert_eq!(
                inspect(
                    &encode(&webm(61.0, Some(500_000_000), unknown)),
                    "audio/webm"
                ),
                Some(Duration::from_millis(30_500))
            );
        }
        for value in [f64::NAN, f64::INFINITY, -1.0, 0.0] {
            assert!(inspect(&encode(&webm(value, None, false)), "video/webm").is_none());
        }
        assert!(inspect(&encode(&webm(1.0, Some(0), false)), "video/webm").is_none());
        let valid = webm(30_000.0, None, false);
        for len in 0..valid.len() {
            assert!(inspect(&encode(&valid[..len]), "video/webm").is_none());
        }
        let mut wide_size = valid[..9].to_vec();
        // Eight-byte known and unknown sizes both have a one-bit first byte.
        wide_size.extend_from_slice(&[1, 0, 0, 0, 0, 0, 0, 16]);
        wide_size.extend_from_slice(&valid[10..]);
        assert_eq!(
            inspect(&encode(&wide_size), "video/webm"),
            Some(Duration::from_secs(30))
        );
        wide_size[10..17].fill(0xff);
        assert_eq!(
            inspect(&encode(&wide_size), "video/webm"),
            Some(Duration::from_secs(30))
        );
        let chained = [valid.clone(), valid].concat();
        assert!(inspect(&encode(&chained), "video/webm").is_none());
        let unknown_first = [webm(30_000.0, None, true), webm(3_600_000.0, None, false)].concat();
        assert!(inspect(&encode(&unknown_first), "video/webm").is_none());
        let mut unknown_child = webm(30_000.0, None, true);
        unknown_child.extend_from_slice(&[0x1f, 0x43, 0xb6, 0x75, 0xff]);
        assert!(inspect(&encode(&unknown_child), "video/webm").is_none());
        let mut duplicate_info = webm(30_000.0, None, true);
        duplicate_info.extend_from_slice(&webm(3_600_000.0, None, false)[10..]);
        assert!(inspect(&encode(&duplicate_info), "video/webm").is_none());
        let mut with_cluster = webm(30_000.0, None, true);
        with_cluster.extend_from_slice(&[0x1f, 0x43, 0xb6, 0x75, 0x84, 0, 0, 0, 0]);
        assert_eq!(
            inspect(&encode(&with_cluster), "video/webm"),
            Some(Duration::from_secs(30))
        );
    }

    #[test]
    fn malformed_or_pathological_metadata_is_bounded_and_optional() {
        for data in ["", "=", "====", "A", "AAAA===", "!!!!", "Zg=", "Zg==\n"] {
            assert!(EncodedMedia::new(data).is_none());
        }
        let data = encode(&movie(1, 30, 0));
        assert!(inspect(&data, "text/plain").is_none());
        let mut many_boxes = boxed(b"free", &[]).repeat(MAX_HEADER_BYTES / 4);
        many_boxes.extend(movie(1, 30, 0));
        assert!(inspect(&encode(&many_boxes), "video/mp4").is_none());
        assert_eq!(
            duration_from_units(1, 3),
            Some(Duration::from_nanos(333_333_334))
        );
    }
}
