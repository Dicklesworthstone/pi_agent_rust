//! Losslessly compressed text resources bundled into the shipping binary.
//!
//! These resources are large, immutable source snapshots. `build.rs`
//! deterministically compresses them and this module decodes them on demand.
//! Parse-only callers own a temporary `String`; the repeatedly displayed
//! changelog is retained after its first use. Every caller receives the exact
//! original UTF-8 bytes.

use flate2::read::GzDecoder;
use std::io::Read;
use std::sync::OnceLock;

struct EmbeddedText {
    name: &'static str,
    compressed: &'static [u8],
    raw_len: usize,
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
