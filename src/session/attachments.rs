//! Immutable image and media attachments for JSONL sessions and their V2 index.
//!
//! References exist only in persistence. Native messages, providers, SDK callers,
//! exports, and fork plans retain complete content. Blob publication precedes
//! publication of the referencing entry. No save operation collects old blobs.

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json::value::RawValue;
use sha2::{Digest as _, Sha256};
use std::fs::File;
use std::io::{Read as _, Write};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::file_identity::FileIdentity;
use crate::model::{ContentBlock, UserContent};
use crate::session_store_v2::{self, ArtifactWriteMode};

use super::{MAX_JSONL_LINE_BYTES, SessionEntry, SessionMessage};

const INLINE_BYTES: usize = 64 * 1024;
const MAX_BLOB_BYTES: usize = 64 * 1024 * 1024;
const MAX_REFERENCES: usize = 4096;
const BLOB_READ_BUFFER_BYTES: usize = 16 * 1024;
const ERROR_PREFIX: &str = "PI_SESSION_ATTACHMENT_INVALID:";

fn blob_error(reason: impl std::fmt::Display) -> Error {
    Error::session(format!("{ERROR_PREFIX} {reason}"))
}

pub fn is_attachment_error(error: &Error) -> bool {
    matches!(error, Error::Session(message) if message.starts_with(ERROR_PREFIX))
}

/// Append to the entire filename: `session.jsonl` owns `session.jsonl.blobs`.
pub fn sidecar_path(session_path: &Path) -> PathBuf {
    let mut path = session_path.as_os_str().to_os_string();
    path.push(".blobs");
    PathBuf::from(path)
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
enum Encoding {
    #[serde(rename = "base64")]
    Standard,
    #[serde(rename = "base64NoPad")]
    NoPad,
}

impl Encoding {
    fn encode(self, bytes: &[u8]) -> String {
        match self {
            Self::Standard => STANDARD.encode(bytes),
            Self::NoPad => STANDARD_NO_PAD.encode(bytes),
        }
    }

    fn encoded_len(self, bytes: usize) -> Result<usize> {
        let full = (bytes / 3)
            .checked_mul(4)
            .ok_or_else(|| blob_error("encoded attachment length overflow"))?;
        let tail = match (bytes % 3, self) {
            (0, _) => 0,
            (_, Self::Standard) => 4,
            (remainder, Self::NoPad) => remainder + 1,
        };
        full.checked_add(tail)
            .ok_or_else(|| blob_error("encoded attachment length overflow"))
    }
}

/// Matches the SQLite attachment encoding; an object cannot be mistaken for
/// base64 by an older reader or by a context-free native message decoder.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct BlobRef {
    #[serde(rename = "$piBlob")]
    digest: String,
    size_bytes: usize,
    encoding: Encoding,
}

impl BlobRef {
    fn parse(value: &Value) -> Result<Self> {
        let reference: Self = serde_json::from_value(value.clone())
            .map_err(|_| blob_error("malformed attachment reference"))?;
        reference.validate()
    }

    fn parse_raw(value: &RawValue) -> Result<Self> {
        let reference: Self = serde_json::from_str(value.get())
            .map_err(|_| blob_error("duplicate or malformed attachment reference"))?;
        reference.validate()
    }

    fn validate(self) -> Result<Self> {
        let reference = self;
        let Some(hash) = reference.digest.strip_prefix("sha256:") else {
            return Err(blob_error("unsupported attachment digest"));
        };
        if hash.len() != 64
            || !hash
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(blob_error("invalid attachment digest"));
        }
        if reference.size_bytes == 0 || reference.size_bytes > MAX_BLOB_BYTES {
            return Err(blob_error("attachment byte count exceeds admission limits"));
        }
        Ok(reference)
    }

    fn filename(&self) -> &str {
        // Construction and parsing both establish the algorithm and hex shape.
        &self.digest["sha256:".len()..]
    }
}

fn digest(bytes: &[u8]) -> String {
    format!(
        "sha256:{}",
        crate::package_manager::hex_encode(&Sha256::digest(bytes))
    )
}

fn message_blocks(value: &mut Value) -> Option<&mut Vec<Value>> {
    if value.get("type").and_then(Value::as_str) != Some("message") {
        return None;
    }
    value.get_mut("message")?.get_mut("content")?.as_array_mut()
}

fn is_attachment(block: &Value) -> bool {
    matches!(
        block.get("type").and_then(Value::as_str),
        Some("image" | "media")
    )
}

fn prepare(data: &str) -> Result<Option<(BlobRef, Vec<u8>)>> {
    if data.len() <= INLINE_BYTES || data.len() > Encoding::Standard.encoded_len(MAX_BLOB_BYTES)? {
        return Ok(None);
    }
    let (encoding, bytes) = if let Ok(bytes) = STANDARD.decode(data) {
        (Encoding::Standard, bytes)
    } else if let Ok(bytes) = STANDARD_NO_PAD.decode(data) {
        (Encoding::NoPad, bytes)
    } else {
        return Ok(None);
    };
    if bytes.len() <= INLINE_BYTES
        || bytes.len() > MAX_BLOB_BYTES
        || encoding.encode(&bytes) != data
    {
        return Ok(None);
    }
    Ok(Some((
        BlobRef {
            digest: digest(&bytes),
            size_bytes: bytes.len(),
            encoding,
        },
        bytes,
    )))
}

struct BlobDirectory {
    path: PathBuf,
    handle: File,
    identity: FileIdentity,
}

impl BlobDirectory {
    fn open(session_path: &Path, create: bool) -> Result<Self> {
        let path = sidecar_path(session_path);
        let handle = session_store_v2::open_private_directory(&path, create).map_err(blob_error)?;
        let identity = FileIdentity::of_open_file(&handle).map_err(blob_error)?;
        let directory = Self {
            path,
            handle,
            identity,
        };
        directory.validate()?;
        if create {
            // The directory entry itself must survive before a JSONL reference
            // can commit, including the first save of a new session.
            super::sync_parent_dir(&directory.path).map_err(blob_error)?;
        }
        Ok(directory)
    }

    fn validate(&self) -> Result<()> {
        if FileIdentity::of_open_file(&self.handle).map_err(blob_error)? != self.identity
            || FileIdentity::of_path_nofollow(&self.path).map_err(blob_error)? != self.identity
        {
            return Err(blob_error("attachment directory changed during access"));
        }
        Ok(())
    }

    fn read(&self, reference: &BlobRef) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        self.visit_verified(reference, |chunk| {
            if bytes.is_empty() {
                bytes.reserve_exact(reference.size_bytes);
            }
            bytes.extend_from_slice(chunk);
        })?;
        Ok(bytes)
    }

    /// Index construction and validation need proof of the bytes, not a
    /// base64 copy of them. Keep the same file/directory identity checks as
    /// hydration while admitting at most one fixed-size read buffer.
    fn verify(&self, reference: &BlobRef) -> Result<()> {
        self.visit_verified(reference, |_| {})
    }

    fn visit_verified(&self, reference: &BlobRef, mut visit: impl FnMut(&[u8])) -> Result<()> {
        self.validate()?;
        let path = self.path.join(reference.filename());
        let file = session_store_v2::open_regular_file_for_read(&path)
            .map_err(blob_error)?
            .ok_or_else(|| blob_error("referenced attachment is missing"))?;
        let identity = FileIdentity::of_open_file(&file).map_err(blob_error)?;
        if file.metadata().map_err(blob_error)?.len() != reference.size_bytes as u64 {
            return Err(blob_error("attachment byte count does not match"));
        }
        let mut reader = (&file).take(reference.size_bytes as u64 + 1);
        let mut buffer = [0_u8; BLOB_READ_BUFFER_BYTES];
        let mut hasher = Sha256::new();
        let mut bytes_read = 0usize;
        loop {
            let count = match reader.read(&mut buffer) {
                Ok(count) => count,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(blob_error(error)),
            };
            if count == 0 {
                break;
            }
            bytes_read += count;
            hasher.update(&buffer[..count]);
            visit(&buffer[..count]);
        }
        let observed_digest = format!(
            "sha256:{}",
            crate::package_manager::hex_encode(&hasher.finalize())
        );
        if bytes_read != reference.size_bytes || observed_digest != reference.digest {
            return Err(blob_error("attachment digest or byte count does not match"));
        }
        if FileIdentity::of_open_file(&file).map_err(blob_error)? != identity
            || FileIdentity::of_path_nofollow(&path).map_err(blob_error)? != identity
        {
            return Err(blob_error("attachment changed while being read"));
        }
        self.validate()?;
        Ok(())
    }

    fn store(&self, reference: &BlobRef, bytes: &[u8]) -> Result<()> {
        self.validate()?;
        let path = self.path.join(reference.filename());
        if super::session_path_entry_exists(&path).map_err(blob_error)? {
            if self.read(reference)? != bytes {
                return Err(blob_error("existing attachment has different content"));
            }
            return Ok(());
        }
        let staging = self.path.join(format!(".pending-{}", uuid::Uuid::new_v4()));
        let mut file = session_store_v2::open_regular_file_for_write(
            &staging,
            true,
            ArtifactWriteMode::CreateNew,
        )
        .map_err(blob_error)?;
        file.write_all(bytes).map_err(blob_error)?;
        file.sync_all().map_err(blob_error)?;
        drop(file);
        self.validate()?;
        // Atomic no-replace publication never overwrites an existing digest.
        // A failed save can retain a staging file, but never publishes a ref to
        // it, and never truncates the authoritative session or an older blob.
        session_store_v2::rename_regular_file_no_replace(&staging, &path).map_err(blob_error)?;
        super::sync_parent_dir(&path).map_err(blob_error)?;
        if self.read(reference)? != bytes {
            return Err(blob_error("published attachment has different content"));
        }
        Ok(())
    }
}

/// Used only under the caller's existing session persistence lock.
pub(super) struct EntryEncoder<'a> {
    session_path: &'a Path,
    directory: Option<BlobDirectory>,
}

impl<'a> EntryEncoder<'a> {
    pub(super) const fn new(session_path: &'a Path) -> Self {
        Self {
            session_path,
            directory: None,
        }
    }

    pub(super) fn encode(&mut self, entry: &SessionEntry) -> Result<Value> {
        let mut value = serde_json::to_value(entry)?;
        if let Some(blocks) = message_blocks(&mut value) {
            let mut count = 0usize;
            for block in blocks {
                if !is_attachment(block) {
                    continue;
                }
                let Some(data) = block.get_mut("data") else {
                    continue;
                };
                let encoded = data
                    .as_str()
                    .ok_or_else(|| blob_error("in-memory attachment data must be a string"))?;
                let Some((reference, bytes)) = prepare(encoded)? else {
                    continue;
                };
                count += 1;
                if count > MAX_REFERENCES {
                    return Err(blob_error("too many attachment references in one entry"));
                }
                if self.directory.is_none() {
                    self.directory = Some(BlobDirectory::open(self.session_path, true)?);
                }
                let directory = self.directory.as_ref().expect("directory was initialized");
                directory.store(&reference, &bytes)?;
                *data = serde_json::to_value(reference)?;
            }
        }
        super::validate_jsonl_value_for_write(&value, "stored session entry")?;
        Ok(value)
    }

    pub(super) fn validate(&self) -> Result<()> {
        if let Some(directory) = &self.directory {
            directory.validate()?;
        }
        Ok(())
    }
}

struct JsonSize(usize);

impl Write for JsonSize {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len())
            .filter(|size| *size <= MAX_JSONL_LINE_BYTES)
            .ok_or_else(|| std::io::Error::other("hydrated entry exceeds JSON limit"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn json_size(value: &Value) -> Result<usize> {
    let mut size = JsonSize(0);
    serde_json::to_writer(&mut size, value)
        .map_err(|_| blob_error("hydrated entry exceeds JSON limit"))?;
    Ok(size.0)
}

#[derive(Default)]
struct EntryFingerprint {
    digest: Sha256,
    size: usize,
}

impl Write for EntryFingerprint {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.size = self
            .size
            .checked_add(bytes.len())
            .filter(|size| *size <= MAX_JSONL_LINE_BYTES)
            .ok_or_else(|| std::io::Error::other("hydrated entry exceeds JSON limit"))?;
        self.digest.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Hash the exact native serialization without allocating a serialized copy of
/// the message or its attachments. This is a private persistence reuse check,
/// never an entry ID or a replacement for validating authoritative blob bytes.
pub(super) fn native_entry_fingerprint(entry: &SessionEntry) -> Result<[u8; 32]> {
    let mut writer = EntryFingerprint::default();
    serde_json::to_writer(&mut writer, entry)?;
    Ok(writer.digest.finalize().into())
}

/// Positions refer only to typed native message content, never arbitrary JSON.
#[derive(Debug, Default)]
pub(super) struct ReferencePlan(Vec<(usize, BlobRef)>);

impl ReferencePlan {
    fn build(value: &mut Value) -> Result<Self> {
        let mut expanded = json_size(value)?;
        let mut references = Vec::new();
        if let Some(blocks) = message_blocks(value) {
            for (index, block) in blocks.iter().enumerate() {
                if !is_attachment(block) {
                    continue;
                }
                let Some(data) = block.get("data").filter(|data| !data.is_string()) else {
                    continue;
                };
                if references.len() >= MAX_REFERENCES {
                    return Err(blob_error("too many attachment references in one entry"));
                }
                let reference = BlobRef::parse(data)?;
                expanded = expanded
                    .checked_sub(json_size(data)?)
                    .and_then(|size| {
                        reference
                            .encoding
                            .encoded_len(reference.size_bytes)
                            .ok()
                            .and_then(|length| size.checked_add(length))
                    })
                    .and_then(|size| size.checked_add(2))
                    .filter(|size| *size <= MAX_JSONL_LINE_BYTES)
                    .ok_or_else(|| blob_error("hydrated entry exceeds JSON limit"))?;
                references.push((index, reference));
            }
        }
        Ok(Self(references))
    }

    /// Reconstruct canonical native metadata while retaining compact wire refs.
    /// Used after legacy-ID normalization during V2 migration and verification.
    pub(super) fn payload(&self, entry: &SessionEntry) -> Result<Value> {
        let mut payload = serde_json::to_value(entry)?;
        if let Some(blocks) = message_blocks(&mut payload) {
            for (index, reference) in &self.0 {
                blocks[*index]["data"] = serde_json::to_value(reference)?;
            }
        }
        Ok(payload)
    }
}

/// A validated persistence entry whose attachment bytes remain in their
/// sidecar. The private typed entry is only a schema/metadata projection:
/// referenced data slots are empty there and cannot escape into a Session.
/// Consumers can obtain graph metadata or the complete stored payload only.
#[derive(Debug)]
pub struct StoredEntry {
    entry: SessionEntry,
    references: ReferencePlan,
}

impl StoredEntry {
    pub(crate) const fn base(&self) -> &super::EntryBase {
        self.entry.base()
    }

    pub(crate) const fn entry_type(&self) -> &'static str {
        session_store_v2::session_entry_type(&self.entry)
    }

    pub(super) fn payload(&self) -> Result<Value> {
        self.references.payload(&self.entry)
    }

    pub(super) fn session_name(&self) -> Option<&str> {
        match &self.entry {
            SessionEntry::SessionInfo(info) => info.name.as_deref(),
            _ => None,
        }
    }

    /// Index updates need workdir metadata, never message bodies. In
    /// particular, the private attachment placeholders cannot escape here.
    pub(super) fn workdir_binding(&self) -> Option<&SessionEntry> {
        matches!(&self.entry, SessionEntry::Custom(custom)
            if custom.custom_type.starts_with(crate::session_workdir::WORKDIR_BINDING_PREFIX))
        .then_some(&self.entry)
    }

    /// Produce the same fingerprint as the native entry while retaining only
    /// compact metadata and fixed-size I/O buffers. Base64 is emitted directly
    /// into the hash writer, including the reference's exact padding spelling.
    pub(super) fn native_fingerprint(&self, source: &Path) -> Result<[u8; 32]> {
        if self.references.0.is_empty() {
            return native_entry_fingerprint(&self.entry);
        }

        let json = serde_json::to_string(&self.entry)?;
        let spans = native_attachment_data_spans(&json)?;
        let directory = BlobDirectory::open(source, false)?;
        let mut writer = EntryFingerprint::default();
        let mut cursor = 0usize;
        let mut remaining = self.references.0.iter().peekable();
        for (index, raw) in spans {
            let Some((expected_index, reference)) = remaining.peek().copied() else {
                break;
            };
            if *expected_index != index {
                continue;
            }
            if raw.get() != "\"\"" {
                return Err(blob_error("stored attachment projection is not empty"));
            }
            let start = raw
                .get()
                .as_ptr()
                .addr()
                .checked_sub(json.as_ptr().addr())
                .ok_or_else(|| blob_error("attachment span is outside native metadata"))?;
            let end = start
                .checked_add(raw.get().len())
                .ok_or_else(|| blob_error("attachment metadata span overflow"))?;
            if start < cursor || json.get(start..end) != Some(raw.get()) {
                return Err(blob_error("invalid native attachment metadata span"));
            }
            // Include the opening quote. Base64 uses only JSON-safe ASCII, so
            // streaming its bytes is identical to serde's string encoding.
            writer.write_all(&json.as_bytes()[cursor..=start])?;
            {
                let engine = match reference.encoding {
                    Encoding::Standard => &STANDARD,
                    Encoding::NoPad => &STANDARD_NO_PAD,
                };
                let mut encoder = base64::write::EncoderWriter::new(&mut writer, engine);
                let mut encoding_error = None;
                directory.visit_verified(reference, |bytes| {
                    if encoding_error.is_none() {
                        encoding_error = encoder.write_all(bytes).err();
                    }
                })?;
                if let Some(error) = encoding_error {
                    return Err(blob_error(error));
                }
                encoder.finish().map_err(blob_error)?;
            }
            cursor = end - 1;
            let _ = remaining.next();
        }
        if remaining.next().is_some() {
            return Err(blob_error(
                "attachment reference is outside native metadata",
            ));
        }
        writer.write_all(&json.as_bytes()[cursor..])?;
        Ok(writer.digest.finalize().into())
    }

    /// Materialize an adopted disk entry without re-deserializing metadata:
    /// media name sanitization is intentionally applied only once on read.
    pub(super) fn into_hydrated(self, source: &Path) -> Result<SessionEntry> {
        hydrate_reference_entry(self.entry, &self.references, source)
    }

    pub(super) fn frame_args(&self) -> Result<(String, Option<String>, String, Value)> {
        let base = self.base();
        let id = base
            .id
            .clone()
            .ok_or_else(|| Error::session("stored session entry is missing its ID"))?;
        Ok((
            id,
            base.parent_id.clone(),
            self.entry_type().to_string(),
            self.payload()?,
        ))
    }

    /// Fill only attachment data on a temporary native row. Re-deserializing
    /// the stored payload would apply metadata sanitizers twice and could
    /// change the exact native serialization used by legacy-ID hashing.
    fn hydrate_for_legacy_id(&self, source: &Path) -> Result<SessionEntry> {
        hydrate_reference_entry(self.entry.clone(), &self.references, source)
    }

    /// Legacy IDs hash the native payload, not the stored reference. Hydrate
    /// only the ID-less row being normalized, preserving the existing collision
    /// order, and discard its bytes before advancing to the next row.
    pub(super) fn normalize_and_validate(entries: &mut [Self], source: &Path) -> Result<()> {
        let mut existing = entries
            .iter()
            .filter_map(|entry| entry.base().id.clone())
            .collect::<std::collections::HashSet<_>>();
        for stored in entries.iter_mut() {
            if stored.base().id.is_none() {
                let id = if stored.references.0.is_empty() {
                    super::generate_loaded_entry_id(&stored.entry, &existing)
                } else {
                    let hydrated = stored.hydrate_for_legacy_id(source)?;
                    super::generate_loaded_entry_id(&hydrated, &existing)
                };
                stored.entry.base_mut().id = Some(id.clone());
                existing.insert(id);
            }
        }
        let bases = entries.iter().map(Self::base).collect::<Vec<_>>();
        super::validate_entry_base_graph(&bases, false)
    }
}

fn hydrate_reference_entry(
    mut entry: SessionEntry,
    references: &ReferencePlan,
    source: &Path,
) -> Result<SessionEntry> {
    if references.0.is_empty() {
        return Ok(entry);
    }
    let SessionEntry::Message(message) = &mut entry else {
        return Err(blob_error(
            "attachment reference is outside a native message",
        ));
    };
    let blocks = match &mut message.message {
        SessionMessage::User {
            content: UserContent::Blocks(blocks),
            ..
        }
        | SessionMessage::ToolResult {
            content: blocks, ..
        } => blocks,
        SessionMessage::Assistant { message } => &mut message.content,
        _ => {
            return Err(blob_error(
                "attachment reference is outside native content blocks",
            ));
        }
    };
    let directory = BlobDirectory::open(source, false)?;
    for (index, reference) in &references.0 {
        let data = match blocks.get_mut(*index) {
            Some(ContentBlock::Image(image)) => &mut image.data,
            Some(ContentBlock::Media(media)) => &mut media.data,
            _ => {
                return Err(blob_error(
                    "attachment reference is outside a native data slot",
                ));
            }
        };
        let bytes = directory.read(reference)?;
        *data = reference.encoding.encode(&bytes);
    }
    Ok(entry)
}

/// Validate the native schema and every referenced blob without retaining
/// attachment bytes. Only migration/integrity consumers use this projection;
/// ordinary Session loading continues through the fully hydrating decoder.
pub fn decode_stored_entry(source: Option<&Path>, json: &str) -> Result<StoredEntry> {
    let original_error = match serde_json::from_str::<SessionEntry>(json) {
        Ok(entry) => {
            return Ok(StoredEntry {
                entry,
                references: ReferencePlan::default(),
            });
        }
        Err(error) => error,
    };
    let spans = native_reference_spans(json)?;
    if spans.is_empty() {
        return Err(original_error.into());
    }
    let entry = validate_reference_entry_shape(json, &spans)?;
    let mut value: Value = serde_json::from_str(json)?;
    let references = ReferencePlan::build(&mut value)?;
    let source = source.ok_or_else(|| {
        blob_error("attachment references require the authoritative JSONL source path")
    })?;
    let directory = BlobDirectory::open(source, false)?;
    for (_, reference) in &references.0 {
        directory.verify(reference)?;
    }
    Ok(StoredEntry { entry, references })
}

pub fn decode_entry(source: Option<&Path>, json: &str) -> Result<SessionEntry> {
    decode_entry_with_references(source, json).map(|(entry, _)| entry)
}

/// Validate the shared persistence reference format without reading any blobs.
/// SQLite must apply the same native-envelope validation as JSONL before
/// hydrating a `Value`, which otherwise collapses duplicate member names.
/// Opaque tool arguments and custom data retain their usual JSON semantics.
#[cfg(feature = "sqlite-sessions")]
pub fn validate_native_attachment_references(json: &str) -> Result<()> {
    let spans = native_reference_spans(json)?;
    if !spans.is_empty() {
        validate_reference_entry_shape(json, &spans)?;
    }
    Ok(())
}

/// Keep every member occurrence while inspecting only native content paths.
/// Opaque tool arguments and custom data are never recursively interpreted.
struct RawObject<'a>(Vec<(String, &'a RawValue)>);

impl<'de> Deserialize<'de> for RawObject<'de> {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct ObjectVisitor;

        impl<'de> serde::de::Visitor<'de> for ObjectVisitor {
            type Value = RawObject<'de>;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a JSON object")
            }

            fn visit_map<M>(self, mut map: M) -> std::result::Result<Self::Value, M::Error>
            where
                M: serde::de::MapAccess<'de>,
            {
                let mut members = Vec::new();
                while let Some(member) = map.next_entry::<String, &'de RawValue>()? {
                    members.push(member);
                }
                Ok(RawObject(members))
            }
        }

        deserializer.deserialize_map(ObjectVisitor)
    }
}

impl RawObject<'_> {
    fn has_type(&self, allowed: &[&str]) -> bool {
        self.0.iter().any(|(key, value)| {
            key == "type"
                && serde_json::from_str::<String>(value.get())
                    .is_ok_and(|kind| allowed.contains(&kind.as_str()))
        })
    }
}

fn native_reference_spans(json: &str) -> Result<Vec<&RawValue>> {
    let mut references = Vec::new();
    for (_, data) in native_attachment_data_spans(json)? {
        if data.get().trim_start().starts_with('"') {
            continue;
        }
        if references.len() >= MAX_REFERENCES {
            return Err(blob_error("too many attachment references in one entry"));
        }
        references.push(data);
    }
    Ok(references)
}

fn native_attachment_data_spans(json: &str) -> Result<Vec<(usize, &RawValue)>> {
    let Ok(entry) = serde_json::from_str::<RawObject<'_>>(json) else {
        return Ok(Vec::new());
    };
    if !entry.has_type(&["message"]) {
        return Ok(Vec::new());
    }
    let mut references = Vec::new();
    let mut reference_count = 0usize;
    for (_, message) in entry.0.iter().filter(|(key, _)| key == "message") {
        let Ok(message) = serde_json::from_str::<RawObject<'_>>(message.get()) else {
            continue;
        };
        for (_, content) in message.0.iter().filter(|(key, _)| key == "content") {
            let Ok(blocks) = serde_json::from_str::<Vec<&RawValue>>(content.get()) else {
                continue;
            };
            for (index, block) in blocks.into_iter().enumerate() {
                let Ok(block) = serde_json::from_str::<RawObject<'_>>(block.get()) else {
                    continue;
                };
                if !block.has_type(&["image", "media"]) {
                    continue;
                }
                for (_, data) in block.0.iter().filter(|(key, _)| key == "data") {
                    if !data.get().trim_start().starts_with('"') {
                        reference_count += 1;
                        if reference_count > MAX_REFERENCES {
                            return Err(blob_error("too many attachment references in one entry"));
                        }
                    }
                    references.push((index, *data));
                }
            }
        }
    }
    Ok(references)
}

/// Validate native structure without collapsing duplicate keys into a Value.
/// Replacing only the borrowed native data spans preserves the typed reader's
/// treatment of every other field, including opaque tool/custom JSON values.
fn validate_reference_entry_shape(json: &str, spans: &[&RawValue]) -> Result<SessionEntry> {
    let mut placeholder = String::with_capacity(json.len());
    let mut cursor = 0usize;
    for raw in spans {
        BlobRef::parse_raw(raw)?;
        let start = raw
            .get()
            .as_ptr()
            .addr()
            .checked_sub(json.as_ptr().addr())
            .ok_or_else(|| blob_error("attachment reference is outside its source entry"))?;
        let end = start
            .checked_add(raw.get().len())
            .ok_or_else(|| blob_error("attachment reference span overflow"))?;
        if start < cursor || json.get(start..end) != Some(raw.get()) {
            return Err(blob_error("invalid attachment reference span"));
        }
        placeholder.push_str(&json[cursor..start]);
        placeholder.push_str("\"\"");
        cursor = end;
    }
    placeholder.push_str(&json[cursor..]);
    serde_json::from_str::<SessionEntry>(&placeholder)
        .map_err(|_| blob_error("duplicate or malformed native attachment-bearing entry"))
}

pub(super) fn decode_entry_with_references(
    source: Option<&Path>,
    json: &str,
) -> Result<(SessionEntry, ReferencePlan)> {
    // The common inline path retains the existing typed parser, including
    // duplicate-field handling, and does not open the attachment directory.
    let original_error = match serde_json::from_str::<SessionEntry>(json) {
        Ok(entry) => return Ok((entry, ReferencePlan::default())),
        Err(error) => error,
    };
    let spans = native_reference_spans(json)?;
    if spans.is_empty() {
        return Err(original_error.into());
    }
    validate_reference_entry_shape(json, &spans)?;
    let mut value: Value = serde_json::from_str(json)?;
    let plan = ReferencePlan::build(&mut value)?;
    let source = source.ok_or_else(|| {
        blob_error("attachment references require the authoritative JSONL source path")
    })?;
    let directory = BlobDirectory::open(source, false)?;
    if let Some(blocks) = message_blocks(&mut value) {
        for (index, reference) in &plan.0 {
            let bytes = directory.read(reference)?;
            blocks[*index]["data"] = Value::String(reference.encoding.encode(&bytes));
        }
    }
    let entry = serde_json::from_value(value)
        .map_err(|_| blob_error("invalid native message in attachment-bearing entry"))?;
    Ok((entry, plan))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ContentBlock, ImageContent, MediaContent, UserContent};
    use crate::session::{Session, SessionMessage, V2OpenMode};
    use serde_json::json;
    use std::fs;

    fn run_async<T>(future: impl std::future::Future<Output = T>) -> T {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        runtime.block_on(future)
    }

    fn image(data: String) -> ContentBlock {
        ContentBlock::Image(ImageContent {
            data,
            mime_type: "image/png".to_string(),
        })
    }

    fn media(data: String, mime: &str) -> ContentBlock {
        ContentBlock::Media(MediaContent {
            data,
            mime_type: mime.to_string(),
            name: Some("clip <one>".to_string()),
        })
    }

    fn message(blocks: Vec<ContentBlock>) -> SessionMessage {
        SessionMessage::User {
            content: UserContent::Blocks(blocks),
            timestamp: Some(0),
        }
    }

    fn text_message(text: &str) -> SessionMessage {
        SessionMessage::User {
            content: UserContent::Text(text.to_string()),
            timestamp: Some(0),
        }
    }

    fn session_at(path: &Path) -> Session {
        let mut session = Session::create_with_dir(path.parent().map(Path::to_path_buf));
        session.path = Some(path.to_path_buf());
        session
    }

    fn save(session: &mut Session) {
        run_async(session.save()).expect("save session");
    }

    fn open(path: &Path) -> Result<Session> {
        run_async(Session::open(path.to_string_lossy().as_ref()))
    }

    fn blobs(path: &Path) -> Vec<PathBuf> {
        fs::read_dir(sidecar_path(path))
            .expect("blob directory")
            .map(|entry| entry.expect("blob entry").path())
            .filter(|path| {
                path.file_name().is_some_and(|name| {
                    let name = name.to_string_lossy();
                    name.len() == 64 && name.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
            })
            .collect()
    }

    fn wire_entries(path: &Path) -> Vec<Value> {
        fs::read_to_string(path)
            .expect("JSONL")
            .lines()
            .skip(1)
            .map(|line| serde_json::from_str(line).expect("wire entry"))
            .collect()
    }

    fn rewrite_wire(path: &Path, entries: &[Value]) {
        let stored = fs::read_to_string(path).expect("read source");
        let mut bytes = stored.lines().next().expect("header").as_bytes().to_vec();
        bytes.push(b'\n');
        for entry in entries {
            serde_json::to_writer(&mut bytes, entry).expect("entry JSON");
            bytes.push(b'\n');
        }
        fs::write(path, bytes).expect("write wire fixture");
    }

    #[allow(clippy::needless_pass_by_value)]
    fn assert_attachment_error(error: Error, detail: &str) {
        assert!(
            is_attachment_error(&error),
            "expected named attachment error: {error}"
        );
        assert!(error.to_string().contains(detail), "{error}");
    }

    #[test]
    fn verification_reads_bounded_chunks_and_checks_the_last_byte() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("streaming.jsonl");
        let mut bytes = vec![0x38; INLINE_BYTES + 47];
        let mut session = session_at(&path);
        session.append_message(message(vec![image(STANDARD.encode(&bytes))]));
        save(&mut session);
        let wire = wire_entries(&path);
        let reference =
            BlobRef::parse(&wire[0]["message"]["content"][0]["data"]).expect("stored reference");
        let blob_directory = BlobDirectory::open(&path, false).expect("open blob directory");
        let mut offset = 0usize;
        let mut chunks = 0usize;
        blob_directory
            .visit_verified(&reference, |chunk| {
                assert!(chunk.len() <= BLOB_READ_BUFFER_BYTES);
                assert_eq!(chunk, &bytes[offset..offset + chunk.len()]);
                offset += chunk.len();
                chunks += 1;
            })
            .expect("verify every chunk");
        assert!(chunks > 1);
        assert_eq!(offset, bytes.len());

        *bytes.last_mut().expect("last byte") ^= 1;
        fs::write(blob_directory.path.join(reference.filename()), bytes)
            .expect("corrupt final byte");
        assert_attachment_error(
            blob_directory
                .verify(&reference)
                .expect_err("tail must be hashed"),
            "digest",
        );
    }

    #[test]
    fn real_save_append_rewrite_deduplicates_images_video_and_audio() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("media.jsonl");
        let bytes = vec![0x91; INLINE_BYTES + 3];
        let padded = STANDARD.encode(&bytes);
        let unpadded = STANDARD_NO_PAD.encode(&bytes);
        let mut session = session_at(&path);
        session.append_message(message(vec![
            image(padded.clone()),
            media(unpadded.clone(), "video/mp4"),
        ]));
        save(&mut session);
        session.append_message(message(vec![media(padded.clone(), "audio/wav")]));
        save(&mut session);
        assert_eq!(session.appends_since_checkpoint, 1, "exercise append path");
        assert_eq!(blobs(&path).len(), 1);
        assert_eq!(fs::read(&blobs(&path)[0]).expect("blob bytes"), bytes);

        let wire = wire_entries(&path);
        assert_eq!(
            wire[0]["message"]["content"][0]["data"]["encoding"],
            "base64"
        );
        assert_eq!(
            wire[0]["message"]["content"][1]["data"]["encoding"],
            "base64NoPad"
        );
        assert!(fs::metadata(&path).expect("metadata").len() < 4096);
        assert!(!fs::read_to_string(&path).expect("JSONL").contains(&padded));

        let original = serde_json::to_value(&session.entries).expect("native entries");
        let mut loaded = open(&path).expect("open compact JSONL");
        assert_eq!(serde_json::to_value(&loaded.entries).unwrap(), original);
        loaded.header.provider = Some("test-provider".to_string());
        loaded.header_dirty = true;
        save(&mut loaded);
        assert_eq!(loaded.appends_since_checkpoint, 0, "exercise full rewrite");
        assert_eq!(blobs(&path).len(), 1);
        let reloaded = open(&path).expect("reopen rewritten JSONL");
        assert_eq!(serde_json::to_value(&reloaded.entries).unwrap(), original);
        assert!(reloaded.to_messages().iter().any(|message| {
            serde_json::to_string(message)
                .expect("provider message")
                .contains(&unpadded)
        }));
    }

    #[test]
    fn stored_fingerprints_match_native_serialization_for_every_attachment_role_and_padding() {
        for tail in 0..3 {
            let directory = tempfile::tempdir().expect("tempdir");
            let path = directory.path().join("fingerprints.jsonl");
            let bytes = vec![0xfb; INLINE_BYTES + 5 + tail];
            let blocks = vec![
                ContentBlock::Text(crate::model::TextContent::new("quote \" / \\ \n 雪")),
                image(STANDARD.encode(&bytes)),
                ContentBlock::ToolCall(crate::model::ToolCall {
                    id: "opaque".to_string(),
                    name: "read".to_string(),
                    arguments: json!({
                        "content":[{"type":"image","data":{"$piBlob":"opaque"}}],
                        "escaped":"\"\\\n"
                    }),
                    thought_signature: None,
                }),
                image("legacy \"\\\n not base64".to_string()),
                media(STANDARD_NO_PAD.encode(&bytes), "audio/wav"),
            ];
            let mut session = session_at(&path);
            session.append_message(message(blocks.clone()));
            session.append_message(SessionMessage::Assistant {
                message: crate::model::AssistantMessage {
                    content: blocks.clone(),
                    ..crate::model::AssistantMessage::default()
                },
            });
            session.append_message(SessionMessage::ToolResult {
                tool_call_id: "fingerprint-call".to_string(),
                tool_name: "read_media".to_string(),
                content: blocks,
                details: Some(json!({"type":"media", "data":{"$piBlob":"opaque"}})),
                is_error: false,
                timestamp: Some(0),
            });
            save(&mut session);
            for mut wire in wire_entries(&path) {
                wire["message"]["content"][4]["name"] = json!("\u{0001}  clip");
                let json = wire.to_string();
                let native = decode_entry(Some(&path), &json).expect("native decoder");
                let native_json = serde_json::to_vec(&native).expect("native serialized bytes");
                let expected: [u8; 32] = Sha256::digest(&native_json).into();
                let stored = decode_stored_entry(Some(&path), &json).expect("stored decoder");
                assert_eq!(
                    native_entry_fingerprint(&native).expect("in-memory fingerprint"),
                    expected
                );
                assert_eq!(
                    stored
                        .native_fingerprint(&path)
                        .expect("streaming fingerprint"),
                    expected
                );
                assert_eq!(
                    serde_json::to_vec(&stored.into_hydrated(&path).expect("adopt disk row"))
                        .expect("adopted serialization"),
                    native_json,
                    "adoption must not reapply media name sanitization"
                );
            }
        }
    }

    #[test]
    fn stored_fingerprint_revalidates_a_blob_changed_after_metadata_was_read() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("fingerprint-corruption.jsonl");
        let mut bytes = vec![0x4a; INLINE_BYTES + 7];
        let mut session = session_at(&path);
        session.append_message(message(vec![image(STANDARD.encode(&bytes))]));
        save(&mut session);
        let json = wire_entries(&path)[0].to_string();
        let stored = decode_stored_entry(Some(&path), &json).expect("read valid metadata");
        *bytes.last_mut().expect("final byte") ^= 1;
        fs::write(blobs(&path).pop().expect("blob"), bytes).expect("corrupt final byte");
        assert_attachment_error(
            stored
                .native_fingerprint(&path)
                .expect_err("reuse cannot hide changed bytes"),
            "digest",
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn compact_append_reuses_media_and_adopts_concurrent_branches_and_workdir_metadata() {
        use crate::session::JsonlPersistedEntry;

        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("concurrent-media.jsonl");
        let mut local = session_at(&path);
        local.append_message(message(vec![image(
            STANDARD.encode(vec![0x91; INLINE_BYTES + 2]),
        )]));
        save(&mut local);
        let mut other = open(&path).expect("concurrent session");
        other.session_dir = Some(directory.path().to_path_buf());
        other.append_message(message(vec![media(
            STANDARD_NO_PAD.encode(vec![0x72; INLINE_BYTES + 4]),
            "video/mp4",
        )]));
        other.append_session_info(Some("concurrent name".to_string()));
        let attached_cwd = directory.path().join("attached-workspace");
        other.append_custom_entry(
            crate::session_workdir::WORKDIR_BINDING_TYPE.to_string(),
            Some(json!({
                "originalCwd":other.header.cwd,
                "previousCwd":other.header.cwd,
                "cwd":attached_cwd
            })),
        );
        save(&mut other);
        let committed_before = fs::read(&path).expect("concurrent commit");

        local.append_message(text_message("independent local branch"));
        let local_before = serde_json::to_value(&local.entries).unwrap();
        let reusable = run_async(crate::session::jsonl_reusable_entries(&local.entries)).unwrap();
        let receipt = crate::session::append_jsonl_entries_blocking(
            &path,
            directory.path(),
            &local.header.id,
            local.entries[1..].to_vec(),
            &reusable,
        )
        .expect("append without materializing existing media");
        assert!(matches!(receipt.entries[0], JsonlPersistedEntry::Reuse(0)));
        assert!(matches!(
            receipt.entries[1],
            JsonlPersistedEntry::Hydrated(_)
        ));
        assert!(matches!(receipt.entries[4], JsonlPersistedEntry::Reuse(1)));
        assert_eq!(serde_json::to_value(&local.entries).unwrap(), local_before);
        assert!(fs::read(&path).unwrap().starts_with(&committed_before));

        let (header, entries) = receipt.accept(&mut local.entries).expect("accept receipt");
        local.accept_persisted_state(header, entries);
        assert_eq!(local.entries.len(), 5);
        assert_eq!(local.cached_message_count, 3);
        assert_eq!(local.get_name().as_deref(), Some("concurrent name"));
        assert_eq!(
            crate::session_workdir::binding_path_from_entries(&local.header, &local.entries)
                .expect("adopted workdir binding"),
            attached_cwd
        );
        let index = crate::session_index::SessionIndex::for_sessions_root(directory.path());
        let indexed = index.list_sessions(None).expect("session index");
        let row = indexed
            .iter()
            .find(|row| row.id == local.header.id)
            .expect("indexed session");
        assert_eq!(row.cwd, attached_cwd.display().to_string());
        assert_eq!(row.message_count, 3);
        assert_eq!(row.name.as_deref(), Some("concurrent name"));
        assert_eq!(
            serde_json::to_value(&local.entries).unwrap(),
            serde_json::to_value(open(&path).expect("reopen merged session").entries).unwrap()
        );
        assert_eq!(blobs(&path).len(), 2);
    }

    #[test]
    fn compact_append_recovers_changed_public_payloads_instead_of_reusing_them() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("mutated-media.jsonl");
        let mut session = session_at(&path);
        let original = STANDARD.encode(vec![0x91; INLINE_BYTES + 2]);
        session.append_message(message(vec![image(original.clone())]));
        save(&mut session);
        let mut mutated = serde_json::to_value(&session.entries[0]).unwrap();
        mutated["message"]["content"][0]["data"] =
            json!(STANDARD.encode(vec![0x92; INLINE_BYTES + 2]));
        session.entries[0] = serde_json::from_value(mutated).unwrap();
        session.append_message(text_message("keep authoritative history"));
        save(&mut session);
        assert_eq!(session.appends_since_checkpoint, 1);
        assert_eq!(
            serde_json::to_value(&session.entries[0]).unwrap()["message"]["content"][0]["data"],
            original
        );
        assert_eq!(blobs(&path).len(), 1);
        assert_eq!(
            serde_json::to_value(&session.entries).unwrap(),
            serde_json::to_value(open(&path).expect("reopen authoritative history").entries)
                .unwrap()
        );
    }

    #[test]
    fn compact_media_append_survives_a_lost_receipt_and_rejects_changed_replay() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("lost-media-receipt.jsonl");
        let mut session = session_at(&path);
        session.append_message(text_message("root"));
        save(&mut session);
        session.append_message(message(vec![media(
            STANDARD_NO_PAD.encode(vec![0x91; INLINE_BYTES + 2]),
            "audio/wav",
        )]));
        let replay = session.entries[1].clone();
        let reusable = run_async(crate::session::jsonl_reusable_entries(&session.entries)).unwrap();
        let local_before = serde_json::to_value(&session.entries).unwrap();
        let receipt = crate::session::append_jsonl_entries_blocking(
            &path,
            directory.path(),
            &session.header.id,
            vec![replay.clone()],
            &reusable,
        )
        .expect("writer reaches disk before its owner accepts the receipt");
        drop(receipt);
        assert_eq!(
            serde_json::to_value(&session.entries).unwrap(),
            local_before
        );
        assert_eq!(
            session
                .persisted_entry_count
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        let committed = fs::read(&path).unwrap();

        let mut changed = serde_json::to_value(replay).unwrap();
        changed["message"]["content"][0]["data"] =
            json!(STANDARD_NO_PAD.encode(vec![0x92; INLINE_BYTES + 2]));
        let error = crate::session::append_jsonl_entries_blocking(
            &path,
            directory.path(),
            &session.header.id,
            vec![serde_json::from_value(changed).unwrap()],
            &reusable,
        )
        .expect_err("same ID with different bytes must reject replay");
        assert!(
            error
                .to_string()
                .contains("conflicting persisted and in-memory payloads")
        );
        assert_eq!(fs::read(&path).unwrap(), committed);
        save(&mut session);
        assert_eq!(session.entries.len(), 2);
        assert_eq!(
            fs::read(&path).unwrap(),
            committed,
            "identical replay appends no bytes"
        );
        assert_eq!(
            serde_json::to_value(&session.entries).unwrap(),
            local_before
        );
    }

    #[test]
    fn cancelling_compact_snapshot_before_writer_admission_keeps_all_payloads() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("cancel-media-snapshot.jsonl");
        let mut session = session_at(&path);
        session.append_message(message(vec![image(
            STANDARD.encode(vec![0x91; INLINE_BYTES + 2]),
        )]));
        save(&mut session);
        let committed = fs::read(&path).unwrap();
        session.append_message(text_message("not admitted yet"));
        let local_before = serde_json::to_value(&session.entries).unwrap();
        run_async(async {
            let mut saving = std::pin::pin!(session.save());
            assert!(
                futures::poll!(saving.as_mut()).is_pending(),
                "fingerprint collection must yield before admitting the writer"
            );
        });
        assert_eq!(fs::read(&path).unwrap(), committed);
        assert_eq!(
            serde_json::to_value(&session.entries).unwrap(),
            local_before
        );
        assert_eq!(
            session
                .persisted_entry_count
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        save(&mut session);
        assert_eq!(
            open(&path)
                .expect("retry after snapshot cancellation")
                .entries
                .len(),
            2
        );
    }

    #[test]
    fn fork_copy_and_html_export_own_complete_media_without_parent_access() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("parent.jsonl");
        let data = STANDARD.encode(vec![0x42; INLINE_BYTES + 5]);
        let mut parent = session_at(&path);
        parent.append_message(message(vec![
            image(data.clone()),
            media(data.clone(), "video/mp4"),
            media(data.clone(), "audio/wav"),
        ]));
        let fork_target = parent.append_message(text_message("try another path"));
        save(&mut parent);
        let parent = open(&path).expect("hydrate parent");
        let snapshot = parent.export_snapshot();

        let copied_path = directory.path().join("copy.jsonl");
        let mut copied = parent.clone();
        copied.path = Some(copied_path.clone());
        save(&mut copied);
        let forked_path = directory.path().join("fork.jsonl");
        let mut forked = session_at(&forked_path);
        forked.init_from_fork_plan(parent.plan_fork_from_user_message(&fork_target).unwrap());
        save(&mut forked);
        fs::rename(
            sidecar_path(&path),
            directory.path().join("parent-blobs-offline"),
        )
        .expect("make parent attachments unavailable");

        assert_attachment_error(open(&path).expect_err("parent must fail"), ERROR_PREFIX);
        let copy = open(&copied_path).expect("copy owns attachments");
        let fork = open(&forked_path).expect("fork owns attachments");
        assert_eq!(copy.entries.len(), 2);
        assert_eq!(fork.entries.len(), 1);
        assert_eq!(blobs(&copied_path).len(), 1);
        assert_eq!(blobs(&forked_path).len(), 1);
        assert_eq!(
            serde_json::to_value(&copy.entries[0]).unwrap(),
            serde_json::to_value(&fork.entries[0]).unwrap()
        );
        let html = snapshot.to_html();
        assert!(html.contains(&format!("data:image/png;base64,{data}")));
        assert!(html.contains(&format!("data:video/mp4;base64,{data}")));
        assert!(html.contains(&format!("data:audio/wav;base64,{data}")));
        assert!(html.contains("<video controls"));
        assert!(html.contains("<audio controls"));
        assert!(!html.contains("$piBlob"));
        assert!(!html.contains("clip <one>"));
    }

    #[test]
    fn inline_historical_and_opaque_data_are_preserved_without_sidecar() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("inline.jsonl");
        let invalid = "not-base64!".repeat(INLINE_BYTES / 5);
        let mut session = session_at(&path);
        session.append_message(message(vec![
            image(STANDARD.encode(vec![7; INLINE_BYTES])),
            media(invalid.clone(), "video/mp4"),
            ContentBlock::ToolCall(crate::model::ToolCall {
                id: "opaque-call".to_string(),
                name: "not-native-content".to_string(),
                arguments: json!({"type":"media", "data":{"$piBlob":"../../private"}}),
                thought_signature: None,
            }),
        ]));
        session.append_custom_entry(
            "opaque".to_string(),
            Some(json!({
                "type":"media", "data":{"$piBlob":"../../private"}
            })),
        );
        save(&mut session);
        assert!(!sidecar_path(&path).exists());
        let loaded = open(&path).expect("legacy and opaque data remain valid");
        assert_eq!(
            serde_json::to_value(&loaded.entries).unwrap(),
            serde_json::to_value(&session.entries).unwrap()
        );
        assert!(fs::read_to_string(&path).unwrap().contains(&invalid));
    }

    #[test]
    fn malformed_digest_size_and_encoding_refs_are_hard_errors() {
        for mutation in [
            json!({"$piBlob":"sha256:../../outside", "sizeBytes":70000, "encoding":"base64"}),
            json!({
                "$piBlob":format!("sha256:{}", "A".repeat(64)),
                "sizeBytes":70000, "encoding":"base64"
            }),
            json!({
                "$piBlob":format!("sha256:{}", "0".repeat(64)),
                "sizeBytes":0, "encoding":"base64"
            }),
            json!({
                "$piBlob":format!("sha256:{}", "0".repeat(64)),
                "sizeBytes":MAX_BLOB_BYTES + 1, "encoding":"base64"
            }),
            json!({
                "$piBlob":format!("sha256:{}", "0".repeat(64)),
                "sizeBytes":70000, "encoding":"urlBase64"
            }),
            json!({
                "$piBlob":format!("sha256:{}", "0".repeat(64)),
                "sizeBytes":70000, "encoding":"base64", "path":"../outside"
            }),
        ] {
            let directory = tempfile::tempdir().expect("tempdir");
            let path = directory.path().join("invalid.jsonl");
            let mut session = session_at(&path);
            session.append_message(message(vec![image(
                STANDARD.encode(vec![1; INLINE_BYTES + 2]),
            )]));
            save(&mut session);
            let mut wire = wire_entries(&path);
            wire[0]["message"]["content"][0]["data"] = mutation;
            rewrite_wire(&path, &wire);
            assert_attachment_error(
                open(&path).expect_err("bad ref cannot become skipped history"),
                ERROR_PREFIX,
            );
            assert_attachment_error(
                decode_stored_entry(Some(&path), &wire[0].to_string())
                    .expect_err("migration must reject the same malformed reference"),
                ERROR_PREFIX,
            );
        }
    }

    #[test]
    fn duplicate_native_fields_cannot_hide_a_reference_in_either_order() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("duplicates.jsonl");
        let mut session = session_at(&path);
        session.append_message(message(vec![image(
            STANDARD.encode(vec![0x23; INLINE_BYTES + 2]),
        )]));
        save(&mut session);
        let wire = wire_entries(&path).pop().unwrap();
        let reference = wire["message"]["content"][0]["data"].to_string();
        let content = wire["message"]["content"].to_string();
        let original_message = wire["message"].to_string();
        let entry_prefix = r#"{"type":"message","id":"duplicate",
            "timestamp":"2026-01-01T00:00:00Z","message":"#;
        let duplicate_data = [
            format!(r#""data":{reference},"data":"AA==""#),
            format!(r#""data":"AA==","data":{reference}"#),
        ];
        for data in duplicate_data {
            let source = format!(
                r#"{entry_prefix}{{"role":"user","content":[
                    {{"type":"image","mimeType":"image/png",{data}}}]}}}}"#
            );
            assert_attachment_error(
                decode_entry(Some(&path), &source).expect_err("duplicate data is never skipped"),
                "duplicate",
            );
            assert_attachment_error(
                decode_stored_entry(Some(&path), &source)
                    .expect_err("stored projection cannot collapse duplicate data"),
                "duplicate",
            );
        }
        let duplicate_content =
            format!(r#"{entry_prefix}{{"role":"user","content":{content},"content":"hidden"}}}}"#);
        let duplicate_message = format!(
            r#"{entry_prefix}{original_message},"message":{{"role":"user","content":"hidden"}}}}"#
        );
        let duplicate_type = format!(
            r#"{{"type":"message","type":"custom","id":"duplicate",
                "timestamp":"2026-01-01T00:00:00Z",
                "message":{original_message},"customType":"opaque"}}"#
        );
        for source in [duplicate_content, duplicate_message, duplicate_type] {
            assert_attachment_error(
                decode_entry(Some(&path), &source).expect_err("duplicate wrapper cannot hide ref"),
                "duplicate",
            );
            assert_attachment_error(
                decode_stored_entry(Some(&path), &source)
                    .expect_err("stored projection cannot collapse duplicate wrappers"),
                "duplicate",
            );
        }
    }

    #[test]
    fn native_reference_validation_preserves_opaque_tool_json_semantics() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("tool-result.jsonl");
        let data = STANDARD.encode(vec![0x39; INLINE_BYTES + 2]);
        let details = json!({"type":"media", "data":{"$piBlob":"../opaque"}});
        let mut session = session_at(&path);
        session.append_message(SessionMessage::Assistant {
            message: crate::model::AssistantMessage {
                content: vec![image(data.clone())],
                ..crate::model::AssistantMessage::default()
            },
        });
        session.append_message(SessionMessage::ToolResult {
            tool_call_id: "call-1".to_string(),
            tool_name: "read_media".to_string(),
            content: vec![media(data, "audio/wav")],
            details: Some(details.clone()),
            is_error: false,
            timestamp: Some(1),
        });
        save(&mut session);
        assert_eq!(blobs(&path).len(), 1);
        let wire = wire_entries(&path);
        assert!(wire[0]["message"]["content"][0]["data"].is_object());
        assert_eq!(wire[1]["message"]["details"], details);
        assert_eq!(
            serde_json::to_value(open(&path).unwrap().entries).unwrap(),
            serde_json::to_value(&session.entries).unwrap()
        );
        let with_duplicate_details = wire[1].to_string().replacen(
            &format!("\"details\":{details}"),
            r#""details":{"type":"media","data":{"$piBlob":"../opaque"},"key":1,"key":2}"#,
            1,
        );
        let decoded = decode_entry(Some(&path), &with_duplicate_details)
            .expect("opaque Value fields retain normal last-member behavior");
        assert_eq!(
            serde_json::to_value(&decoded).unwrap()["message"]["details"]["key"],
            2
        );
        let stored = decode_stored_entry(Some(&path), &with_duplicate_details)
            .expect("stored projection retains opaque Value semantics");
        assert_eq!(stored.payload().unwrap()["message"]["details"]["key"], 2);
        assert_eq!(
            stored
                .native_fingerprint(&path)
                .expect("opaque JSON fingerprint"),
            native_entry_fingerprint(&decoded).expect("native opaque JSON fingerprint")
        );
    }

    #[test]
    fn missing_corrupt_and_replaced_blobs_fail_and_cannot_be_repaired_by_save() {
        for scenario in ["missing", "wrong-size", "wrong-hash", "replaced"] {
            let directory = tempfile::tempdir().expect("tempdir");
            let path = directory.path().join("broken.jsonl");
            let bytes = vec![0x43; INLINE_BYTES + 2];
            let mut session = session_at(&path);
            session.append_message(message(vec![image(STANDARD.encode(&bytes))]));
            save(&mut session);
            let store = crate::session::create_v2_sidecar_from_jsonl(&path)
                .expect("index valid attachment before damaging it");
            let blob = blobs(&path).pop().expect("blob");
            match scenario {
                "missing" => fs::rename(&blob, directory.path().join("unavailable")).unwrap(),
                "wrong-size" => fs::write(&blob, b"short").unwrap(),
                "wrong-hash" => fs::write(&blob, vec![0x44; bytes.len()]).unwrap(),
                _ => {
                    fs::rename(&blob, directory.path().join("old-object")).unwrap();
                    let mut replacement = session_store_v2::open_regular_file_for_write(
                        &blob,
                        true,
                        ArtifactWriteMode::CreateNew,
                    )
                    .unwrap();
                    replacement.write_all(&vec![0x45; bytes.len()]).unwrap();
                }
            }
            let source = fs::read(&path).unwrap();
            assert_attachment_error(open(&path).expect_err(scenario), ERROR_PREFIX);
            assert_attachment_error(
                store
                    .validate_session_integrity_with_source(Some(&path))
                    .expect_err("compact frame verification must reject damaged bytes"),
                ERROR_PREFIX,
            );
            assert_attachment_error(
                crate::session::verify_v2_against_jsonl(&path, &store)
                    .expect_err("compact source verification must reject damaged bytes"),
                ERROR_PREFIX,
            );
            assert_attachment_error(
                crate::session::create_v2_sidecar_from_jsonl(&path)
                    .expect_err("migration must reject damaged bytes"),
                ERROR_PREFIX,
            );
            session.append_message(text_message("must not overwrite damaged source"));
            assert_attachment_error(run_async(session.save()).expect_err(scenario), ERROR_PREFIX);
            assert_eq!(fs::read(&path).unwrap(), source);
        }
    }

    #[test]
    fn failed_new_blob_publication_leaves_committed_jsonl_unchanged() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("publication.jsonl");
        let mut session = session_at(&path);
        session.append_message(text_message("committed before media"));
        save(&mut session);
        let original = fs::read(&path).unwrap();
        let encoded = STANDARD.encode(vec![0x61; INLINE_BYTES + 2]);
        let (reference, bytes) = prepare(&encoded).unwrap().unwrap();
        let blobs = BlobDirectory::open(&path, true).unwrap();
        let target = blobs.path.join(reference.filename());
        let mut collision = session_store_v2::open_regular_file_for_write(
            &target,
            true,
            ArtifactWriteMode::CreateNew,
        )
        .unwrap();
        collision.write_all(&vec![0x62; bytes.len()]).unwrap();
        drop(collision);
        session.append_message(message(vec![media(encoded, "video/mp4")]));
        assert_attachment_error(
            run_async(session.save()).expect_err("immutable corruption cannot be healed"),
            "digest",
        );
        assert_eq!(fs::read(&path).unwrap(), original);
        assert_eq!(fs::read(&target).unwrap(), vec![0x62; bytes.len()]);
        assert_eq!(
            open(&path).unwrap().entries.len(),
            1,
            "unreferenced damaged blob does not hide committed text"
        );
    }

    #[test]
    fn reference_expansion_and_count_are_validated_before_disk_access() {
        let reference = json!({
            "$piBlob": format!("sha256:{}", "0".repeat(64)),
            "sizeBytes": MAX_BLOB_BYTES,
            "encoding":"base64"
        });
        let huge = json!({
            "type":"message", "timestamp":"2026-01-01T00:00:00Z",
            "message":{"role":"user", "content":[
                {"type":"image", "mimeType":"image/png", "data":reference},
                {"type":"image", "mimeType":"image/png", "data":reference}
            ]}
        });
        assert_attachment_error(
            decode_entry(None, &huge.to_string())
                .expect_err("expansion rejected before source path"),
            "hydrated entry exceeds JSON limit",
        );
        let tiny = json!({"type":"image", "mimeType":"image/png", "data":{
            "$piBlob":format!("sha256:{}", "0".repeat(64)), "sizeBytes":1, "encoding":"base64"
        }});
        let many = json!({
            "type":"message",
            "message":{"role":"user", "content":vec![tiny; MAX_REFERENCES + 1]}
        });
        assert_attachment_error(
            decode_entry(None, &many.to_string())
                .expect_err("reference count rejected before source path"),
            "too many attachment references",
        );
    }

    #[cfg(unix)]
    #[test]
    fn sidecar_and_blob_symlinks_cannot_be_followed_or_written_through() {
        use std::os::unix::fs::symlink;

        for swap_directory in [true, false] {
            let directory = tempfile::tempdir().expect("tempdir");
            let path = directory.path().join("linked.jsonl");
            let mut session = session_at(&path);
            session.append_message(message(vec![image(
                STANDARD.encode(vec![0x71; INLINE_BYTES + 2]),
            )]));
            save(&mut session);
            let source = fs::read(&path).unwrap();
            let target = if swap_directory {
                sidecar_path(&path)
            } else {
                blobs(&path).pop().unwrap()
            };
            let parked = directory.path().join("outside");
            fs::rename(&target, &parked).unwrap();
            symlink(&parked, &target).unwrap();
            assert_attachment_error(open(&path).expect_err("symlink read denied"), ERROR_PREFIX);
            session.append_message(text_message("denied write"));
            assert_attachment_error(
                run_async(session.save()).expect_err("symlink write denied"),
                ERROR_PREFIX,
            );
            assert_eq!(fs::read(&path).unwrap(), source);
            assert!(
                fs::symlink_metadata(&target)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
        }
    }

    #[test]
    fn large_jsonl_parse_propagates_attachment_errors_instead_of_skipping_rows() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("parallel.jsonl");
        let mut session = session_at(&path);
        for index in 0..=crate::session::PARALLEL_THRESHOLD {
            session.append_message(text_message(&format!("text {index}")));
        }
        session.append_message(message(vec![image(
            STANDARD.encode(vec![0x72; INLINE_BYTES + 2]),
        )]));
        save(&mut session);
        let blob = blobs(&path).pop().unwrap();
        fs::rename(&blob, directory.path().join("missing-blob")).unwrap();
        assert_attachment_error(
            open(&path).expect_err("parallel workers must propagate attachment failure"),
            "missing",
        );
    }

    #[cfg(unix)]
    #[test]
    fn terminal_session_symlink_uses_target_attachments_in_both_parse_paths() {
        use std::os::unix::fs::symlink;

        for text_count in [0, crate::session::PARALLEL_THRESHOLD] {
            let directory = tempfile::tempdir().expect("tempdir");
            let target = directory.path().join("target.jsonl");
            let link = directory.path().join("alias.jsonl");
            let mut session = session_at(&target);
            for index in 0..text_count {
                session.append_message(text_message(&format!("entry {index}")));
            }
            let data = STANDARD.encode(vec![0x6a; INLINE_BYTES + 2]);
            session.append_message(message(vec![image(data.clone())]));
            save(&mut session);
            symlink(&target, &link).unwrap();
            let shadow = BlobDirectory::open(&link, true).unwrap();
            let shadow_file = shadow.path.join(blobs(&target)[0].file_name().unwrap());
            fs::write(&shadow_file, b"untrusted alias attachment").unwrap();

            // Call the blocking boundary directly: the async public open also
            // resolves the terminal link, so it alone would hide this defect.
            let (mut opened, diagnostics) =
                crate::session::open_jsonl_blocking(&link).expect("resolve target sidecar");
            assert!(diagnostics.skipped_entries.is_empty());
            assert_eq!(opened.entries.len(), text_count + 1);
            assert_eq!(
                opened.path.as_ref(),
                Some(&fs::canonicalize(&target).unwrap())
            );
            assert!(
                serde_json::to_string(&opened.entries)
                    .unwrap()
                    .contains(&data)
            );
            opened.header.provider = Some("updated-through-target".to_string());
            opened.header_dirty = true;
            save(&mut opened);
            assert!(
                fs::symlink_metadata(&link)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            assert_eq!(
                fs::read(&shadow_file).unwrap(),
                b"untrusted alias attachment"
            );
            assert_eq!(open(&link).unwrap().entries.len(), text_count + 1);
        }
    }

    #[test]
    fn v2_keeps_compact_refs_and_rehydrates_hidden_branch_before_rewrite() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("branches.jsonl");
        let mut session = session_at(&path);
        let root =
            session.append_message(message(vec![image(
                STANDARD.encode(vec![0x31; INLINE_BYTES + 2]),
            )]));
        let active = session.append_message(text_message("active branch"));
        assert!(session.navigate_to(&root));
        let hidden = session.append_message(message(vec![media(
            STANDARD.encode(vec![0x32; INLINE_BYTES + 2]),
            "video/mp4",
        )]));
        assert!(session.navigate_to(&active));
        save(&mut session);
        let wire = wire_entries(&path);
        let source_entries = wire
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        let compact_entries = crate::session::read_jsonl_entries_for_v2(
            &mut std::io::Cursor::new(source_entries),
            &path,
        )
        .expect("parse compact entries for migration");
        assert_eq!(compact_entries.len(), wire.len());
        for (stored, original) in compact_entries.iter().zip(&wire) {
            assert!(
                serde_json::to_vec(&stored.entry).unwrap().len() < 2048,
                "graph validation must not retain hydrated attachment strings"
            );
            assert_eq!(stored.payload().unwrap(), *original);
        }
        let store =
            crate::session::create_v2_sidecar_from_jsonl(&path).expect("migrate compact source");
        let verification = crate::session::verify_v2_against_jsonl(&path, &store).unwrap();
        assert!(
            verification.entry_count_match
                && verification.hash_chain_match
                && verification.index_consistent
        );
        let frames = store.read_all_entries().unwrap();
        assert!(frames[0].payload.get().contains("$piBlob"));
        assert!(frames[2].payload.get().contains("$piBlob"));
        assert!(frames.iter().all(|frame| frame.payload.get().len() < 2048));
        for (frame, original) in frames.iter().zip(&session.entries) {
            let hydrated = session_store_v2::frame_to_session_entry_with_source(frame, Some(&path))
                .expect("public session entries contain complete native attachments");
            assert_eq!(
                serde_json::to_value(hydrated).unwrap(),
                serde_json::to_value(original).unwrap()
            );
        }
        assert_attachment_error(
            session_store_v2::frame_to_session_entry(&frames[0])
                .expect_err("context-free decoder cannot guess source"),
            "authoritative JSONL source path",
        );

        let (mut partial, diagnostics) = Session::open_from_v2_with_source(
            &store,
            session.header.clone(),
            V2OpenMode::ActivePath,
            Some(&path),
        )
        .unwrap();
        assert!(diagnostics.skipped_entries.is_empty());
        assert_eq!(partial.entries.len(), 2);
        assert!(partial.get_entry(&hidden).is_none());
        partial.path = Some(path.clone());
        partial.v2_sidecar_root = Some(session_store_v2::v2_sidecar_path(&path));
        partial.header.provider = Some("changed".to_string());
        partial.header_dirty = true;
        save(&mut partial);
        assert!(
            partial.get_entry(&hidden).is_some(),
            "hidden media must survive full hydration"
        );
        assert_eq!(partial.leaf_id(), Some(active.as_str()));
        assert_eq!(wire_entries(&path).len(), 3);
        assert_eq!(blobs(&path).len(), 2);
        assert_eq!(open(&path).unwrap().entries.len(), 3);
        let dry_run =
            crate::session::migrate_dry_run(&path).expect("portable dry-run source context");
        assert!(dry_run.entry_count_match && dry_run.hash_chain_match && dry_run.index_consistent);
    }

    #[test]
    fn legacy_media_migration_preserves_native_ids_and_collision_order() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("legacy-media.jsonl");
        let bytes = vec![0x49; INLINE_BYTES + 3];
        let mut session = session_at(&path);
        session.append_message(message(vec![
            image(STANDARD.encode(&bytes)),
            media(STANDARD_NO_PAD.encode(&bytes), "audio/wav"),
        ]));
        save(&mut session);
        let mut legacy = wire_entries(&path).pop().unwrap();
        let object = legacy.as_object_mut().unwrap();
        object.remove("id");
        object.remove("parentId");
        let native = decode_entry(Some(&path), &legacy.to_string()).expect("hydrate legacy row");
        let reserved_id =
            crate::session::generate_loaded_entry_id(&native, &std::collections::HashSet::new());
        let mut reserved = legacy.clone();
        reserved["id"] = Value::String(reserved_id.clone());
        reserved["message"]["content"] =
            Value::String("reserve the first hash candidate".to_string());
        rewrite_wire(&path, &[legacy.clone(), reserved, legacy]);

        let (native_session, diagnostics) =
            crate::session::open_jsonl_blocking(&path).expect("load native legacy ID contract");
        assert!(diagnostics.skipped_entries.is_empty());
        let expected_ids = native_session
            .entries
            .iter()
            .map(|entry| entry.base().id.clone().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(expected_ids[1], reserved_id);
        assert_ne!(expected_ids[0], reserved_id);
        assert_ne!(expected_ids[0], expected_ids[2]);

        let store = crate::session::create_v2_sidecar_from_jsonl(&path)
            .expect("migrate ID-less image and media rows");
        let frames = store.read_all_entries().unwrap();
        assert_eq!(
            frames
                .iter()
                .map(|frame| frame.entry_id.clone())
                .collect::<Vec<_>>(),
            expected_ids
        );
        assert!(frames.iter().all(|frame| frame.payload.get().len() < 2048));
        for (frame, expected) in frames.iter().zip(&native_session.entries) {
            let hydrated = session_store_v2::frame_to_session_entry_with_source(frame, Some(&path))
                .expect("rehydrate migrated media row");
            assert_eq!(
                serde_json::to_value(hydrated).unwrap(),
                serde_json::to_value(expected).unwrap()
            );
        }
        let verification = crate::session::verify_v2_against_jsonl(&path, &store).unwrap();
        assert!(verification.entry_count_match && verification.hash_chain_match);
    }

    #[test]
    fn legacy_media_ids_apply_metadata_deserializers_exactly_once() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("legacy-media-metadata.jsonl");
        let data = STANDARD_NO_PAD.encode(vec![0x4b; INLINE_BYTES + 3]);
        let mut session = session_at(&path);
        session.append_message(message(vec![media(data.clone(), "audio/wav")]));
        session.append_message(SessionMessage::Assistant {
            message: crate::model::AssistantMessage {
                content: vec![media(data.clone(), "audio/wav")],
                ..crate::model::AssistantMessage::default()
            },
        });
        session.append_message(SessionMessage::ToolResult {
            tool_call_id: "legacy-media-call".to_string(),
            tool_name: "read_media".to_string(),
            content: vec![media(data, "audio/wav")],
            details: None,
            is_error: false,
            timestamp: Some(0),
        });
        save(&mut session);
        let mut wire = wire_entries(&path);
        for entry in &mut wire {
            let object = entry.as_object_mut().unwrap();
            object.remove("id");
            object.remove("parentId");
            // Trimming precedes control removal: one deserialize yields
            // "  clip", while deserializing that result again yields "clip".
            entry["message"]["content"][0]["name"] = json!("\u{0001}  clip");
        }
        rewrite_wire(&path, &wire);
        let (native_session, diagnostics) =
            crate::session::open_jsonl_blocking(&path).expect("load original legacy metadata");
        assert!(diagnostics.skipped_entries.is_empty());
        for entry in &native_session.entries {
            assert_eq!(
                serde_json::to_value(entry).unwrap()["message"]["content"][0]["name"],
                "  clip"
            );
        }

        let store = crate::session::create_v2_sidecar_from_jsonl(&path)
            .expect("migrate without re-deserializing native metadata");
        let frames = store.read_all_entries().unwrap();
        assert_eq!(frames.len(), native_session.entries.len());
        for (frame, native) in frames.iter().zip(&native_session.entries) {
            assert_eq!(Some(&frame.entry_id), native.base_id());
            let payload: Value = serde_json::from_str(frame.payload.get()).unwrap();
            assert_eq!(payload["message"]["content"][0]["name"], "  clip");
        }
        let verification = crate::session::verify_v2_against_jsonl(&path, &store).unwrap();
        assert!(
            verification.entry_count_match
                && verification.hash_chain_match
                && verification.index_consistent
        );
    }

    #[test]
    fn compact_migration_rejects_duplicate_ids_missing_parents_and_cycles() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("invalid-graph.jsonl");
        let mut session = session_at(&path);
        session.append_message(message(vec![image(
            STANDARD.encode(vec![0x4a; INLINE_BYTES + 2]),
        )]));
        save(&mut session);
        let mut root = wire_entries(&path).pop().unwrap();
        root["id"] = json!("root");
        root["parentId"] = Value::Null;
        let mut missing_parent = root.clone();
        missing_parent["parentId"] = json!("missing");
        let mut self_cycle = root.clone();
        self_cycle["parentId"] = json!("root");
        let mut cycle_root = root.clone();
        cycle_root["parentId"] = json!("child");
        let mut cycle_child = root.clone();
        cycle_child["id"] = json!("child");
        cycle_child["parentId"] = json!("root");
        for (entries, detail) in [
            (vec![root.clone(), root], "duplicate entry ID"),
            (vec![missing_parent], "missing parent"),
            (vec![self_cycle], "cycle"),
            (vec![cycle_root, cycle_child], "cycle"),
        ] {
            let source = entries
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n");
            let error =
                crate::session::read_jsonl_entries_for_v2(&mut std::io::Cursor::new(source), &path)
                    .expect_err("compact references cannot weaken strict graph validation");
            assert!(error.to_string().contains(detail), "{error}");
        }
    }

    #[test]
    fn warm_v2_cannot_hide_a_missing_blob_or_replaced_source() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("warm.jsonl");
        let mut session = session_at(&path);
        session.append_message(message(vec![image(
            STANDARD.encode(vec![0x51; INLINE_BYTES + 2]),
        )]));
        save(&mut session);
        let store = crate::session::create_v2_sidecar_from_jsonl(&path).unwrap();
        assert!(
            open(&path).unwrap().v2_sidecar_root.is_some(),
            "exercise healthy V2 resume first"
        );
        let before = store.read_all_entries().unwrap()[0]
            .payload
            .get()
            .to_string();
        let blob = blobs(&path).pop().unwrap();
        let parked = directory.path().join("offline");
        fs::rename(&blob, &parked).unwrap();
        assert_attachment_error(
            open(&path).expect_err("warm V2 must still verify referenced bytes"),
            "missing",
        );
        assert_eq!(store.read_all_entries().unwrap()[0].payload.get(), before);
        fs::rename(&parked, &blob).unwrap();
        let mut replacement = session_at(&directory.path().join("replacement.jsonl"));
        replacement.append_message(text_message("new authoritative session"));
        save(&mut replacement);
        fs::write(&path, fs::read(replacement.path.as_ref().unwrap()).unwrap()).unwrap();
        let opened = open(&path).expect("changed source invalidates old cache");
        assert_eq!(opened.header.id, replacement.header.id);
        assert!(opened.v2_sidecar_root.is_none());
        assert!(
            !serde_json::to_string(&opened.entries)
                .unwrap()
                .contains("$piBlob")
        );
    }

    #[test]
    fn session_listing_does_not_hydrate_blobs_but_open_does() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("listing.jsonl");
        let mut session = session_at(&path);
        session.append_message(message(vec![image(
            STANDARD.encode(vec![0x65; INLINE_BYTES + 2]),
        )]));
        save(&mut session);
        let blob = blobs(&path).pop().unwrap();
        fs::rename(&blob, directory.path().join("unavailable")).unwrap();
        assert_eq!(
            crate::session::load_session_meta_jsonl(&path)
                .unwrap()
                .message_count,
            1
        );
        assert_attachment_error(
            open(&path).expect_err("listing is not attachment validation"),
            "missing",
        );
    }
}
