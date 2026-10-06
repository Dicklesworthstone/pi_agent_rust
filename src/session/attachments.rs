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
use crate::session_store_v2::{self, ArtifactWriteMode};

use super::{MAX_JSONL_LINE_BYTES, SessionEntry};

const INLINE_BYTES: usize = 64 * 1024;
const MAX_BLOB_BYTES: usize = 64 * 1024 * 1024;
const MAX_REFERENCES: usize = 4096;
const ERROR_PREFIX: &str = "PI_SESSION_ATTACHMENT_INVALID:";

fn blob_error(reason: impl std::fmt::Display) -> Error {
    Error::session(format!("{ERROR_PREFIX} {reason}"))
}

pub(crate) fn is_attachment_error(error: &Error) -> bool {
    matches!(error, Error::Session(message) if message.starts_with(ERROR_PREFIX))
}

/// Append to the entire filename: `session.jsonl` owns `session.jsonl.blobs`.
pub(crate) fn sidecar_path(session_path: &Path) -> PathBuf {
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
        let handle =
            session_store_v2::open_private_directory(&path, create).map_err(blob_error)?;
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
        self.validate()?;
        let path = self.path.join(reference.filename());
        let file = session_store_v2::open_regular_file_for_read(&path)
            .map_err(blob_error)?
            .ok_or_else(|| blob_error("referenced attachment is missing"))?;
        let identity = FileIdentity::of_open_file(&file).map_err(blob_error)?;
        if file.metadata().map_err(blob_error)?.len() != reference.size_bytes as u64 {
            return Err(blob_error("attachment byte count does not match"));
        }
        let mut bytes = Vec::with_capacity(reference.size_bytes);
        (&file)
            .take(reference.size_bytes as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(blob_error)?;
        if bytes.len() != reference.size_bytes || digest(&bytes) != reference.digest {
            return Err(blob_error("attachment digest or byte count does not match"));
        }
        if FileIdentity::of_path_nofollow(&path).map_err(blob_error)? != identity {
            return Err(blob_error("attachment changed while being read"));
        }
        self.validate()?;
        Ok(bytes)
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
        session_store_v2::rename_regular_file_no_replace(&staging, &path)
            .map_err(blob_error)?;
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

/// Positions refer only to typed native message content, never arbitrary JSON.
#[derive(Default)]
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

pub(crate) fn decode_entry(source: Option<&Path>, json: &str) -> Result<SessionEntry> {
    decode_entry_with_references(source, json).map(|(entry, _)| entry)
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
    let Ok(entry) = serde_json::from_str::<RawObject<'_>>(json) else {
        return Ok(Vec::new());
    };
    if !entry.has_type(&["message"]) {
        return Ok(Vec::new());
    }
    let mut references = Vec::new();
    for (_, message) in entry.0.iter().filter(|(key, _)| key == "message") {
        let Ok(message) = serde_json::from_str::<RawObject<'_>>(message.get()) else {
            continue;
        };
        for (_, content) in message.0.iter().filter(|(key, _)| key == "content") {
            let Ok(blocks) = serde_json::from_str::<Vec<&RawValue>>(content.get()) else {
                continue;
            };
            for block in blocks {
                let Ok(block) = serde_json::from_str::<RawObject<'_>>(block.get()) else {
                    continue;
                };
                if !block.has_type(&["image", "media"]) {
                    continue;
                }
                for (_, data) in block.0.iter().filter(|(key, _)| key == "data") {
                    if data.get().trim_start().starts_with('"') {
                        continue;
                    }
                    if references.len() >= MAX_REFERENCES {
                        return Err(blob_error("too many attachment references in one entry"));
                    }
                    references.push(*data);
                }
            }
        }
    }
    Ok(references)
}

/// Validate native structure without collapsing duplicate keys into a Value.
/// Replacing only the borrowed native data spans preserves the typed reader's
/// treatment of every other field, including opaque tool/custom JSON values.
fn validate_reference_entry_shape(json: &str, spans: &[&RawValue]) -> Result<()> {
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
        .map_err(|_| blob_error("duplicate or malformed native attachment-bearing entry"))?;
    Ok(())
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

    fn assert_attachment_error(error: Error, detail: &str) {
        assert!(
            is_attachment_error(&error),
            "expected named attachment error: {error}"
        );
        assert!(error.to_string().contains(detail), "{error}");
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
        assert_eq!(wire[0]["message"]["content"][0]["data"]["encoding"], "base64");
        assert_eq!(wire[0]["message"]["content"][1]["data"]["encoding"], "base64NoPad");
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
            serde_json::to_string(message).expect("provider message").contains(&unpadded)
        }));
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
        fs::rename(sidecar_path(&path), directory.path().join("parent-blobs-offline"))
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
        session.append_custom_entry("opaque".to_string(), Some(json!({
            "type":"media", "data":{"$piBlob":"../../private"}
        })));
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
        }
        let duplicate_content = format!(
            r#"{entry_prefix}{{"role":"user","content":{content},"content":"hidden"}}}}"#
        );
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
            serde_json::to_value(decoded).unwrap()["message"]["details"]["key"],
            2
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
                {"type":"image", "mimeType":"image/png", "data":reference.clone()},
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
            assert!(fs::symlink_metadata(&target).unwrap().file_type().is_symlink());
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
        session.append_message(message(vec![image(STANDARD.encode(vec![0x72; INLINE_BYTES + 2]))]));
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
            assert_eq!(opened.path.as_ref(), Some(&fs::canonicalize(&target).unwrap()));
            assert!(serde_json::to_string(&opened.entries).unwrap().contains(&data));
            opened.header.provider = Some("updated-through-target".to_string());
            opened.header_dirty = true;
            save(&mut opened);
            assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
            assert_eq!(fs::read(&shadow_file).unwrap(), b"untrusted alias attachment");
            assert_eq!(open(&link).unwrap().entries.len(), text_count + 1);
        }
    }

    #[test]
    fn v2_keeps_compact_refs_and_rehydrates_hidden_branch_before_rewrite() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("branches.jsonl");
        let mut session = session_at(&path);
        let root = session.append_message(message(vec![image(
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
    fn warm_v2_cannot_hide_a_missing_blob_or_replaced_source() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("warm.jsonl");
        let mut session = session_at(&path);
        session.append_message(message(vec![image(STANDARD.encode(vec![0x51; INLINE_BYTES + 2]))]));
        save(&mut session);
        let store = crate::session::create_v2_sidecar_from_jsonl(&path).unwrap();
        assert!(
            open(&path).unwrap().v2_sidecar_root.is_some(),
            "exercise healthy V2 resume first"
        );
        let before = store.read_all_entries().unwrap()[0].payload.get().to_string();
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
        assert!(!serde_json::to_string(&opened.entries).unwrap().contains("$piBlob"));
    }

    #[test]
    fn session_listing_does_not_hydrate_blobs_but_open_does() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("listing.jsonl");
        let mut session = session_at(&path);
        session.append_message(message(vec![image(STANDARD.encode(vec![0x65; INLINE_BYTES + 2]))]));
        save(&mut session);
        let blob = blobs(&path).pop().unwrap();
        fs::rename(&blob, directory.path().join("unavailable")).unwrap();
        assert_eq!(crate::session::load_session_meta_jsonl(&path).unwrap().message_count, 1);
        assert_attachment_error(
            open(&path).expect_err("listing is not attachment validation"),
            "missing",
        );
    }
}
