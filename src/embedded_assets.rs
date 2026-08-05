//! Losslessly compressed text resources bundled into the shipping binary.
//!
//! These resources are large, immutable source snapshots. File-backed assets
//! are deterministically compressed by `build.rs`; inline JavaScript literals
//! use the const LZSS codec below. This module decodes both forms on demand.
//! Parse-only callers own a temporary `String`; repeatedly used resources are
//! retained by their owning `OnceLock`. Every caller receives the exact
//! original UTF-8 bytes.

use flate2::read::GzDecoder;
use std::io::Read;
use std::sync::OnceLock;

struct EmbeddedText {
    name: &'static str,
    compressed: &'static [u8],
    raw_len: usize,
}

// A 4 Ki-entry table keeps compile-time evaluation bounded while retaining
// nearly all of the compression benefit on the bundled JavaScript corpus.
// Larger tables made every target spend minutes initializing const state.
const LZSS_HASH_SLOTS: usize = 1 << 12;
const LZSS_MAX_DISTANCE: usize = 65_535;
const LZSS_MIN_MATCH: usize = 4;
const LZSS_MAX_MATCH: usize = LZSS_MIN_MATCH + 0x7f;
const LZSS_LITERAL_CHUNK: usize = 0x80;

const fn lzss_hash(input: &[u8], position: usize) -> usize {
    let mut sequence_bytes = [0; std::mem::size_of::<usize>()];
    sequence_bytes[0] = input[position];
    sequence_bytes[1] = input[position + 1];
    sequence_bytes[2] = input[position + 2];
    let sequence = usize::from_le_bytes(sequence_bytes);
    (sequence.wrapping_mul(2_654_435_761) >> 16) & (LZSS_HASH_SLOTS - 1)
}

const fn lzss_match(
    input: &[u8],
    positions: &mut [usize; LZSS_HASH_SLOTS],
    position: usize,
) -> (usize, usize) {
    if position + LZSS_MIN_MATCH > input.len() {
        return (0, 0);
    }
    let hash = lzss_hash(input, position);
    let candidate = positions[hash];
    positions[hash] = position;
    if candidate == usize::MAX || candidate >= position || position - candidate > LZSS_MAX_DISTANCE
    {
        return (0, 0);
    }
    let available = input.len() - position;
    let limit = if available < LZSS_MAX_MATCH {
        available
    } else {
        LZSS_MAX_MATCH
    };
    let mut length = 0;
    while length < limit && input[candidate + length] == input[position + length] {
        length += 1;
    }
    if length < LZSS_MIN_MATCH {
        (0, 0)
    } else {
        (length, position - candidate)
    }
}

const fn lzss_record_match_positions(
    input: &[u8],
    positions: &mut [usize; LZSS_HASH_SLOTS],
    position: usize,
    match_length: usize,
) {
    let mut cursor = position + 1;
    let end = position + match_length;
    while cursor < end {
        if cursor + LZSS_MIN_MATCH <= input.len() {
            positions[lzss_hash(input, cursor)] = cursor;
        }
        cursor += 1;
    }
}

const fn lzss_literal_encoded_len(length: usize) -> usize {
    length + length.div_ceil(LZSS_LITERAL_CHUNK)
}

/// Return the size of the deterministic LZSS representation used for large
/// JavaScript literals. This is evaluated by rustc; the raw literal is not
/// needed by the release binary.
#[expect(
    clippy::large_stack_arrays,
    reason = "the hash table exists only during compile-time evaluation of embedded literals"
)]
pub const fn lzss_compressed_len(input: &[u8]) -> usize {
    let mut positions = [usize::MAX; LZSS_HASH_SLOTS];
    let mut position = 0;
    let mut literal_start = 0;
    let mut encoded_len = 0;
    while position < input.len() {
        let (match_length, _) = lzss_match(input, &mut positions, position);
        if match_length == 0 {
            position += 1;
            continue;
        }
        encoded_len += lzss_literal_encoded_len(position - literal_start) + 3;
        lzss_record_match_positions(input, &mut positions, position, match_length);
        position += match_length;
        literal_start = position;
    }
    encoded_len + lzss_literal_encoded_len(input.len() - literal_start)
}

/// Encode a text literal into the deterministic LZSS representation.
#[expect(
    clippy::large_stack_arrays,
    reason = "the hash table exists only during compile-time evaluation of embedded literals"
)]
pub const fn lzss_compress<const OUTPUT_LEN: usize>(input: &[u8]) -> [u8; OUTPUT_LEN] {
    let mut positions = [usize::MAX; LZSS_HASH_SLOTS];
    let mut output = [0; OUTPUT_LEN];
    let mut output_position = 0;
    let mut position = 0;
    let mut literal_start = 0;
    while position < input.len() {
        let (match_length, match_distance) = lzss_match(input, &mut positions, position);
        if match_length == 0 {
            position += 1;
            continue;
        }
        let mut literal_position = literal_start;
        while literal_position < position {
            let remaining = position - literal_position;
            let chunk_len = if remaining < LZSS_LITERAL_CHUNK {
                remaining
            } else {
                LZSS_LITERAL_CHUNK
            };
            output[output_position] = (chunk_len - 1).to_le_bytes()[0];
            output_position += 1;
            let literal_end = literal_position + chunk_len;
            while literal_position < literal_end {
                output[output_position] = input[literal_position];
                output_position += 1;
                literal_position += 1;
            }
        }
        output[output_position] = 0x80 | (match_length - LZSS_MIN_MATCH).to_le_bytes()[0];
        let match_distance_bytes = match_distance.to_le_bytes();
        output[output_position + 1] = match_distance_bytes[0];
        output[output_position + 2] = match_distance_bytes[1];
        output_position += 3;
        lzss_record_match_positions(input, &mut positions, position, match_length);
        position += match_length;
        literal_start = position;
    }
    let mut literal_position = literal_start;
    while literal_position < input.len() {
        let remaining = input.len() - literal_position;
        let chunk_len = if remaining < LZSS_LITERAL_CHUNK {
            remaining
        } else {
            LZSS_LITERAL_CHUNK
        };
        output[output_position] = (chunk_len - 1).to_le_bytes()[0];
        output_position += 1;
        let literal_end = literal_position + chunk_len;
        while literal_position < literal_end {
            output[output_position] = input[literal_position];
            output_position += 1;
            literal_position += 1;
        }
    }
    assert!(
        output_position == OUTPUT_LEN,
        "LZSS encoded length mismatch"
    );
    output
}

/// Decode a compile-time LZSS literal and fail closed on malformed metadata.
pub fn lzss_decompress(input: &[u8], expected_len: usize) -> Result<String, String> {
    let mut output = Vec::with_capacity(expected_len);
    let mut position = 0;
    while position < input.len() {
        let header = input[position];
        position += 1;
        if header & 0x80 == 0 {
            let length = usize::from(header) + 1;
            let end = position
                .checked_add(length)
                .filter(|end| *end <= input.len())
                .ok_or_else(|| "truncated LZSS literal run".to_string())?;
            output
                .len()
                .checked_add(length)
                .filter(|decoded_len| *decoded_len <= expected_len)
                .ok_or_else(|| "LZSS output exceeds declared length".to_string())?;
            output.extend_from_slice(&input[position..end]);
            position = end;
        } else {
            let distance_end = position
                .checked_add(2)
                .filter(|end| *end <= input.len())
                .ok_or_else(|| "truncated LZSS match distance".to_string())?;
            let distance = usize::from(u16::from_le_bytes([input[position], input[position + 1]]));
            position = distance_end;
            if distance == 0 || distance > output.len() {
                return Err("invalid LZSS match distance".to_string());
            }
            let length = usize::from(header & 0x7f) + LZSS_MIN_MATCH;
            output
                .len()
                .checked_add(length)
                .filter(|decoded_len| *decoded_len <= expected_len)
                .ok_or_else(|| "LZSS output exceeds declared length".to_string())?;
            for _ in 0..length {
                output.push(output[output.len() - distance]);
            }
        }
    }
    if output.len() != expected_len {
        return Err(format!(
            "LZSS output length mismatch: expected {expected_len}, decoded {}",
            output.len()
        ));
    }
    String::from_utf8(output).map_err(|error| format!("LZSS output is not UTF-8: {error}"))
}

impl EmbeddedText {
    const fn new(name: &'static str, compressed: &'static [u8], raw_len: usize) -> Self {
        Self {
            name,
            compressed,
            raw_len,
        }
    }

    fn decode(&self) -> String {
        let mut decoder = GzDecoder::new(self.compressed);
        let mut text = String::with_capacity(self.raw_len);
        if let Err(error) = decoder.read_to_string(&mut text) {
            panic!("embedded text resource {} is corrupt: {error}", self.name);
        }
        assert_eq!(
            text.len(),
            self.raw_len,
            "embedded text resource {} decoded to the wrong length",
            self.name
        );
        text
    }
}

include!(concat!(env!("OUT_DIR"), "/embedded-text-metadata.rs"));

static LEGACY_MODELS_GENERATED_TS: EmbeddedText = EmbeddedText::new(
    "legacy models.generated.ts",
    include_bytes!(concat!(env!("OUT_DIR"), "/legacy-models-generated.ts.gz")),
    LEGACY_MODELS_GENERATED_TS_RAW_LEN,
);
static PROVIDER_UPSTREAM_MODEL_IDS_JSON: EmbeddedText = EmbeddedText::new(
    "provider upstream model IDs",
    include_bytes!(concat!(
        env!("OUT_DIR"),
        "/provider-upstream-model-ids.json.gz"
    )),
    PROVIDER_UPSTREAM_MODEL_IDS_JSON_RAW_LEN,
);
static EXTENSION_ARTIFACT_PROVENANCE_JSON: EmbeddedText = EmbeddedText::new(
    "extension artifact provenance",
    include_bytes!(concat!(
        env!("OUT_DIR"),
        "/extension-artifact-provenance.json.gz"
    )),
    EXTENSION_ARTIFACT_PROVENANCE_JSON_RAW_LEN,
);
static CHANGELOG: EmbeddedText = EmbeddedText::new(
    "changelog",
    include_bytes!(concat!(env!("OUT_DIR"), "/changelog.md.gz")),
    CHANGELOG_RAW_LEN,
);
static CHANGELOG_DECODED: OnceLock<String> = OnceLock::new();

pub fn legacy_models_generated_ts() -> String {
    LEGACY_MODELS_GENERATED_TS.decode()
}

pub const fn legacy_models_generated_ts_crc32c() -> u32 {
    LEGACY_MODELS_GENERATED_TS_CRC32C
}

pub fn provider_upstream_model_ids_json() -> String {
    PROVIDER_UPSTREAM_MODEL_IDS_JSON.decode()
}

pub const fn provider_upstream_model_ids_json_crc32c() -> u32 {
    PROVIDER_UPSTREAM_MODEL_IDS_JSON_CRC32C
}

pub fn extension_artifact_provenance_json() -> String {
    EXTENSION_ARTIFACT_PROVENANCE_JSON.decode()
}

pub fn changelog() -> &'static str {
    CHANGELOG_DECODED.get_or_init(|| CHANGELOG.decode())
}

#[cfg(test)]
mod tests {
    const LZSS_FIXTURE: &str = "literal-prefix:0123456789\nliteral-prefix:0123456789\n尾\n";
    const LZSS_FIXTURE_LEN: usize = super::lzss_compressed_len(LZSS_FIXTURE.as_bytes());
    const LZSS_FIXTURE_ENCODED: [u8; LZSS_FIXTURE_LEN] =
        super::lzss_compress::<LZSS_FIXTURE_LEN>(LZSS_FIXTURE.as_bytes());
    const EMPTY_LEN: usize = super::lzss_compressed_len(b"");
    const EMPTY: [u8; EMPTY_LEN] = super::lzss_compress::<EMPTY_LEN>(b"");

    #[test]
    fn compile_time_lzss_round_trips_and_compresses_repetition() {
        let decoded = super::lzss_decompress(&LZSS_FIXTURE_ENCODED, LZSS_FIXTURE.len())
            .expect("decode compile-time LZSS fixture");
        assert_eq!(decoded, LZSS_FIXTURE);
        assert!(LZSS_FIXTURE_ENCODED.len() < LZSS_FIXTURE.len());

        assert_eq!(super::lzss_decompress(&EMPTY, 0).as_deref(), Ok(""));
    }

    #[test]
    fn lzss_decoder_rejects_truncation_invalid_distance_and_length_drift() {
        assert!(super::lzss_decompress(&[0, b'a'], 2).is_err());
        assert!(super::lzss_decompress(&[0, b'a'], 0).is_err());
        assert!(super::lzss_decompress(&[0x80, 0, 0], 4).is_err());
        assert!(super::lzss_decompress(&[0x80], 4).is_err());
        assert!(super::lzss_decompress(&[0x80, 1, 0], 3).is_err());
        assert!(super::lzss_decompress(&[0, 0xff], 1).is_err());
    }

    #[test]
    fn lzss_round_trips_and_decodes_literal_and_match_boundaries() {
        macro_rules! assert_round_trip {
            ($input:expr) => {{
                const INPUT: &[u8] = $input;
                const ENCODED_LEN: usize = super::lzss_compressed_len(INPUT);
                const ENCODED: [u8; ENCODED_LEN] = super::lzss_compress::<ENCODED_LEN>(INPUT);
                assert_eq!(
                    super::lzss_decompress(&ENCODED, INPUT.len())
                        .expect("decode boundary fixture")
                        .as_bytes(),
                    INPUT
                );
            }};
        }

        assert_round_trip!(b"");
        assert_round_trip!(b"x");
        assert_round_trip!(b"xxx");
        assert_round_trip!(b"xxxx");
        assert_round_trip!(&[b'x'; 127]);
        assert_round_trip!(&[b'x'; 128]);
        assert_round_trip!(&[b'x'; 129]);
        assert_round_trip!(&[b'x'; 131]);
        assert_round_trip!(&[b'x'; 132]);
        assert_round_trip!(&[b'x'; 256]);

        let mut literal_127 = vec![126];
        literal_127.extend(std::iter::repeat_n(b'a', 127));
        assert_eq!(
            super::lzss_decompress(&literal_127, 127)
                .expect("decode 127-byte literal")
                .len(),
            127
        );

        let mut literal_128 = vec![127];
        literal_128.extend(std::iter::repeat_n(b'b', 128));
        assert_eq!(
            super::lzss_decompress(&literal_128, 128)
                .expect("decode 128-byte literal")
                .len(),
            128
        );

        let mut literal_129 = vec![127];
        literal_129.extend(std::iter::repeat_n(b'c', 128));
        literal_129.extend_from_slice(&[0, b'd']);
        assert_eq!(
            super::lzss_decompress(&literal_129, 129)
                .expect("decode split 129-byte literal")
                .len(),
            129
        );

        assert_eq!(
            super::lzss_decompress(&[0, b'a', 0x86, 1, 0], 11).as_deref(),
            Ok("aaaaaaaaaaa")
        );

        let mut distance_boundary = Vec::with_capacity(65_536 + 515);
        for _ in 0..511 {
            distance_boundary.push(0x7f);
            distance_boundary.extend(std::iter::repeat_n(b'x', 128));
        }
        distance_boundary.push(0x7e);
        distance_boundary.extend(std::iter::repeat_n(b'x', 127));
        distance_boundary.extend_from_slice(&[0x80, 0xff, 0xff]);
        assert_eq!(
            super::lzss_decompress(&distance_boundary, 65_539)
                .expect("decode maximum-distance match")
                .len(),
            65_539
        );
    }

    #[test]
    fn compressed_resources_restore_exact_source_bytes() {
        assert_eq!(
            super::legacy_models_generated_ts().as_bytes(),
            include_bytes!("../legacy_pi_mono_code/pi-mono/packages/ai/src/models.generated.ts")
        );
        assert_eq!(
            super::legacy_models_generated_ts_crc32c(),
            crc32c::crc32c(include_bytes!(
                "../legacy_pi_mono_code/pi-mono/packages/ai/src/models.generated.ts"
            ))
        );
        assert_eq!(
            super::provider_upstream_model_ids_json().as_bytes(),
            include_bytes!("../docs/provider-upstream-model-ids-snapshot.json")
        );
        assert_eq!(
            super::provider_upstream_model_ids_json_crc32c(),
            crc32c::crc32c(include_bytes!(
                "../docs/provider-upstream-model-ids-snapshot.json"
            ))
        );
        assert_eq!(
            super::extension_artifact_provenance_json().as_bytes(),
            include_bytes!("../docs/extension-artifact-provenance.json")
        );
        assert_eq!(
            super::changelog().as_bytes(),
            include_bytes!("../CHANGELOG.md")
        );
    }
}
