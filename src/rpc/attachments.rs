//! Borrowed RPC media projection and bounded attachment retrieval.
//!
//! Native sessions, providers and extensions keep their complete content.
//! Only explicitly negotiated RPC serialization replaces admitted image/media
//! data with references. The catalog retains decoded bytes, never filesystem
//! paths, and performs no file or network I/O. There is no eviction: issued
//! references remain readable until the active session changes. When admission
//! fails, serialization preserves the complete original inline data instead.
//!
//! The byte limit bounds retained payloads, not every RPC frame or peak memory.
//! Registration may temporarily decode one bounded new blob; a chunk read pins
//! its source while encoding at most MAX_CHUNK_BYTES. Unchanged base64 sources
//! use a bounded fingerprint cache and are not repeatedly decoded.

use crate::model::{
    AssistantMessage, ContentBlock, Message, StopDetails, StopReason, Usage, UserContent,
};
use crate::session::SessionMessage;
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Serialize, Serializer};
use sha2::{Digest as _, Sha256};
use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

pub(super) const MAX_ATTACHMENT_BYTES: usize = 64 * 1024 * 1024;
pub(super) const MAX_RETAINED_ATTACHMENT_BYTES: usize = 128 * 1024 * 1024;
pub(super) const MAX_CATALOG_ATTACHMENTS: usize = 4096;
pub(super) const MAX_CHUNK_BYTES: usize = 1024 * 1024;
const MAX_ATTACHMENT_ID_BYTES: usize = 128;

type Fingerprint = [u8; 32];

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
enum Encoding {
    #[serde(rename = "base64")]
    Standard,
    #[serde(rename = "base64NoPad")]
    NoPad,
}

/// RPC references share the persistent content digest and encoding metadata.
/// The additional opaque ID scopes retrieval to this catalog generation.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(super) struct AttachmentRef {
    pub attachment_id: String,
    #[serde(rename = "$piBlob")]
    pub digest: String,
    pub size_bytes: usize,
    encoding: Encoding,
}

struct StoredBlob {
    digest: String,
    bytes: Arc<[u8]>,
}

struct CatalogState {
    generation: Uuid,
    blobs: HashMap<String, StoredBlob>,
    // Strict canonical base64 permits at most padded and unpadded spellings
    // per blob, so this map is bounded by twice the admitted blob count.
    encoded: HashMap<Fingerprint, AttachmentRef>,
    retained_bytes: usize,
}

impl CatalogState {
    fn new() -> Self {
        Self {
            generation: Uuid::new_v4(),
            blobs: HashMap::new(),
            encoded: HashMap::new(),
            retained_bytes: 0,
        }
    }
}

#[derive(Clone, Copy)]
struct CatalogLimits {
    blob_bytes: usize,
    retained_bytes: usize,
    entries: usize,
}

impl Default for CatalogLimits {
    fn default() -> Self {
        Self {
            blob_bytes: MAX_ATTACHMENT_BYTES,
            retained_bytes: MAX_RETAINED_ATTACHMENT_BYTES,
            entries: MAX_CATALOG_ATTACHMENTS,
        }
    }
}

/// Shared by one RPC connection's serializers and attachment commands.
/// Mode changes retain the catalog; successful session changes clear it.
#[derive(Clone)]
pub(super) struct AttachmentCatalog {
    state: Arc<Mutex<CatalogState>>,
    limits: CatalogLimits,
}

impl Default for AttachmentCatalog {
    fn default() -> Self {
        Self::with_limits(CatalogLimits::default())
    }
}

impl fmt::Debug for AttachmentCatalog {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AttachmentCatalog { .. }")
    }
}

impl AttachmentCatalog {
    fn with_limits(limits: CatalogLimits) -> Self {
        Self {
            state: Arc::new(Mutex::new(CatalogState::new())),
            limits,
        }
    }

    /// Admit canonical native base64 without cloning its encoded string.
    /// A miss leaves the original inline data untouched in the projection.
    pub(super) fn register(&self, data: &str) -> Option<AttachmentRef> {
        let encoded_limit = self.limits.blob_bytes.div_ceil(3).checked_mul(4)?;
        if data.is_empty() || data.len() > encoded_limit {
            return None;
        }
        let encoded_key: Fingerprint = Sha256::digest(data.as_bytes()).into();
        let generation = {
            let state = self.state.lock().ok()?;
            if let Some(reference) = state.encoded.get(&encoded_key) {
                return Some(reference.clone());
            }
            state.generation
        };

        // These engines reject nonzero trailing bits, whitespace, the URL-safe
        // alphabet and noncanonical padding. No full re-encoding is needed.
        let (encoding, bytes) = if let Ok(bytes) = STANDARD.decode(data) {
            (Encoding::Standard, bytes)
        } else {
            (Encoding::NoPad, STANDARD_NO_PAD.decode(data).ok()?)
        };
        if bytes.is_empty() || bytes.len() > self.limits.blob_bytes {
            return None;
        }
        self.retain(generation, encoded_key, encoding, bytes)
    }

    fn retain(
        &self,
        generation: Uuid,
        encoded_key: Fingerprint,
        encoding: Encoding,
        bytes: Vec<u8>,
    ) -> Option<AttachmentRef> {
        let hash = crate::package_manager::hex_encode(&Sha256::digest(&bytes));
        let digest = format!("sha256:{hash}");
        let attachment_id = format!("rpc:{generation}:{hash}");
        let size_bytes = bytes.len();
        let mut state = self.state.lock().ok()?;
        // A session switch may have happened while the bounded decode ran.
        // An old projection must never repopulate the new session's catalog.
        if state.generation != generation {
            return None;
        }
        if let Some(reference) = state.encoded.get(&encoded_key) {
            return Some(reference.clone());
        }
        if let Some(existing) = state.blobs.get(&attachment_id) {
            if existing.bytes.as_ref() != bytes.as_slice() {
                return None;
            }
        } else {
            let retained_bytes = state.retained_bytes.checked_add(size_bytes)?;
            if state.blobs.len() >= self.limits.entries
                || retained_bytes > self.limits.retained_bytes
            {
                return None;
            }
            state.blobs.insert(
                attachment_id.clone(),
                StoredBlob {
                    digest: digest.clone(),
                    bytes: Arc::from(bytes),
                },
            );
            state.retained_bytes = retained_bytes;
        }
        let reference = AttachmentRef {
            attachment_id,
            digest,
            size_bytes,
            encoding,
        };
        state.encoded.insert(encoded_key, reference.clone());
        Some(reference)
    }

    /// Expire all issued IDs, including IDs for bytes admitted again later.
    pub(super) fn clear(&self) {
        if let Ok(mut state) = self.state.lock() {
            *state = CatalogState::new();
        }
    }

    /// Read raw byte offsets from an issued ID, encoding only the bounded
    /// returned chunk. Decode each response before concatenating its bytes.
    pub(super) fn read(
        &self,
        attachment_id: &str,
        offset: usize,
        max_bytes: usize,
    ) -> Result<AttachmentChunk, AttachmentReadError> {
        if attachment_id.is_empty()
            || attachment_id.len() > MAX_ATTACHMENT_ID_BYTES
            || !attachment_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'-'))
        {
            return Err(AttachmentReadError::InvalidId);
        }
        if max_bytes == 0 || max_bytes > MAX_CHUNK_BYTES {
            return Err(AttachmentReadError::InvalidLength);
        }
        let (digest, bytes) = {
            let state = self
                .state
                .lock()
                .map_err(|_| AttachmentReadError::Unavailable)?;
            let blob = state
                .blobs
                .get(attachment_id)
                .ok_or(AttachmentReadError::UnknownId)?;
            (blob.digest.clone(), Arc::clone(&blob.bytes))
        };
        if offset > bytes.len() {
            return Err(AttachmentReadError::InvalidOffset);
        }
        let count = max_bytes.min(bytes.len() - offset);
        let next_offset = offset + count;
        Ok(AttachmentChunk {
            attachment_id: attachment_id.to_string(),
            digest,
            offset,
            next_offset,
            bytes_returned: count,
            size_bytes: bytes.len(),
            data: STANDARD.encode(&bytes[offset..next_offset]),
            eof: next_offset == bytes.len(),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(super) enum AttachmentReadError {
    #[error("Invalid RPC attachment ID")]
    InvalidId,
    #[error("RPC attachment ID is unknown or expired for the active session")]
    UnknownId,
    #[error("RPC attachment byte offset exceeds the retained payload")]
    InvalidOffset,
    #[error("RPC attachment maxBytes must be between 1 and 1048576")]
    InvalidLength,
    #[error("RPC attachment catalog is unavailable")]
    Unavailable,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AttachmentChunk {
    pub attachment_id: String,
    #[serde(rename = "$piBlob")]
    pub digest: String,
    pub offset: usize,
    pub next_offset: usize,
    pub bytes_returned: usize,
    pub size_bytes: usize,
    pub data: String,
    pub eof: bool,
}

impl fmt::Debug for AttachmentChunk {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AttachmentChunk")
            .field("attachment_id", &self.attachment_id)
            .field("offset", &self.offset)
            .field("bytes_returned", &self.bytes_returned)
            .field("size_bytes", &self.size_bytes)
            .field("eof", &self.eof)
            .finish_non_exhaustive()
    }
}

enum DataProjection<'a> {
    Inline(&'a str),
    Reference(AttachmentRef),
}

impl<'a> DataProjection<'a> {
    fn new(data: &'a str, catalog: &AttachmentCatalog) -> Self {
        catalog
            .register(data)
            .map_or(Self::Inline(data), Self::Reference)
    }
}

impl Serialize for DataProjection<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Inline(data) => data.serialize(serializer),
            Self::Reference(reference) => reference.serialize(serializer),
        }
    }
}

/// Serialize the native block list while replacing only image/media data.
pub(super) struct ContentBlocks<'a> {
    blocks: &'a [ContentBlock],
    catalog: &'a AttachmentCatalog,
}

impl<'a> ContentBlocks<'a> {
    pub(super) const fn new(
        blocks: &'a [ContentBlock],
        catalog: &'a AttachmentCatalog,
    ) -> Self {
        Self { blocks, catalog }
    }
}

impl Serialize for ContentBlocks<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut sequence = serializer.serialize_seq(Some(self.blocks.len()))?;
        for block in self.blocks {
            sequence.serialize_element(&ContentBlockProjection {
                block,
                catalog: self.catalog,
            })?;
        }
        sequence.end()
    }
}

struct ContentBlockProjection<'a> {
    block: &'a ContentBlock,
    catalog: &'a AttachmentCatalog,
}

impl Serialize for ContentBlockProjection<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let (kind, data, mime, name) = match self.block {
            ContentBlock::Image(image) => {
                ("image", image.data.as_str(), image.mime_type.as_str(), None)
            }
            ContentBlock::Media(media) => (
                "media",
                media.data.as_str(),
                media.mime_type.as_str(),
                media.name.as_deref(),
            ),
            // Opaque tool arguments, redacted thinking and custom details are
            // never searched for objects that merely resemble attachments.
            other => return other.serialize(serializer),
        };
        let mut map = serializer.serialize_map(Some(3 + usize::from(name.is_some())))?;
        map.serialize_entry("type", kind)?;
        map.serialize_entry("data", &DataProjection::new(data, self.catalog))?;
        map.serialize_entry("mimeType", mime)?;
        if let Some(name) = name {
            map.serialize_entry("name", name)?;
        }
        map.end()
    }
}

struct UserContentProjection<'a> {
    content: &'a UserContent,
    catalog: &'a AttachmentCatalog,
}

impl Serialize for UserContentProjection<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self.content {
            UserContent::Text(text) => text.serialize(serializer),
            UserContent::Blocks(blocks) => {
                ContentBlocks::new(blocks, self.catalog).serialize(serializer)
            }
        }
    }
}

#[derive(Serialize)]
struct UserMessageView<'a> {
    role: &'static str,
    content: UserContentProjection<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timestamp: Option<i64>,
}

/// An assistant payload without a role, for terminal/partial stream events.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AssistantMessageProjection<'a> {
    content: ContentBlocks<'a>,
    api: &'a str,
    provider: &'a str,
    model: &'a str,
    usage: &'a Usage,
    stop_reason: StopReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop_details: Option<&'a StopDetails>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_message: Option<&'a str>,
    timestamp: i64,
}

impl<'a> AssistantMessageProjection<'a> {
    pub(super) fn new(
        message: &'a AssistantMessage,
        catalog: &'a AttachmentCatalog,
    ) -> Self {
        Self {
            content: ContentBlocks::new(&message.content, catalog),
            api: &message.api,
            provider: &message.provider,
            model: &message.model,
            usage: &message.usage,
            stop_reason: message.stop_reason,
            stop_details: message.stop_details.as_ref(),
            error_message: message.error_message.as_deref(),
            timestamp: message.timestamp,
        }
    }
}

#[derive(Serialize)]
struct AssistantMessageView<'a> {
    role: &'static str,
    #[serde(flatten)]
    message: AssistantMessageProjection<'a>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ToolResultView<'a> {
    role: &'static str,
    tool_call_id: &'a str,
    tool_name: &'a str,
    content: ContentBlocks<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<&'a serde_json::Value>,
    is_error: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    timestamp: Option<i64>,
}

/// Complete native model-message projection; no source payload is cloned.
pub(super) struct ModelMessageProjection<'a> {
    message: &'a Message,
    catalog: &'a AttachmentCatalog,
}

impl<'a> ModelMessageProjection<'a> {
    pub(super) const fn new(
        message: &'a Message,
        catalog: &'a AttachmentCatalog,
    ) -> Self {
        Self { message, catalog }
    }
}

impl Serialize for ModelMessageProjection<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self.message {
            Message::User(user) => UserMessageView {
                role: "user",
                content: UserContentProjection {
                    content: &user.content,
                    catalog: self.catalog,
                },
                timestamp: Some(user.timestamp),
            }
            .serialize(serializer),
            Message::Assistant(assistant) => AssistantMessageView {
                role: "assistant",
                message: AssistantMessageProjection::new(assistant, self.catalog),
            }
            .serialize(serializer),
            Message::ToolResult(result) => ToolResultView {
                role: "toolResult",
                tool_call_id: &result.tool_call_id,
                tool_name: &result.tool_name,
                content: ContentBlocks::new(&result.content, self.catalog),
                details: result.details.as_ref(),
                is_error: result.is_error,
                timestamp: Some(result.timestamp),
            }
            .serialize(serializer),
            Message::Custom(_) => self.message.serialize(serializer),
        }
    }
}

/// Complete session-message projection, including its optional timestamps.
pub(super) struct SessionMessageProjection<'a> {
    message: &'a SessionMessage,
    catalog: &'a AttachmentCatalog,
}

impl<'a> SessionMessageProjection<'a> {
    pub(super) const fn new(
        message: &'a SessionMessage,
        catalog: &'a AttachmentCatalog,
    ) -> Self {
        Self { message, catalog }
    }
}

impl Serialize for SessionMessageProjection<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self.message {
            SessionMessage::User { content, timestamp } => UserMessageView {
                role: "user",
                content: UserContentProjection {
                    content,
                    catalog: self.catalog,
                },
                timestamp: *timestamp,
            }
            .serialize(serializer),
            SessionMessage::Assistant { message } => AssistantMessageView {
                role: "assistant",
                message: AssistantMessageProjection::new(message, self.catalog),
            }
            .serialize(serializer),
            SessionMessage::ToolResult {
                tool_call_id,
                tool_name,
                content,
                details,
                is_error,
                timestamp,
            } => ToolResultView {
                role: "toolResult",
                tool_call_id,
                tool_name,
                content: ContentBlocks::new(content, self.catalog),
                details: details.as_ref(),
                is_error: *is_error,
                timestamp: *timestamp,
            }
            .serialize(serializer),
            SessionMessage::Custom { .. }
            | SessionMessage::BashExecution { .. }
            | SessionMessage::BranchSummary { .. }
            | SessionMessage::CompactionSummary { .. } => self.message.serialize(serializer),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        Cost, CustomMessage, ImageContent, MediaContent, RedactedThinkingContent, TextContent,
        ThinkingContent, ToolCall, ToolResultMessage, UserMessage,
    };
    use serde_json::{Value, json};

    fn small_catalog(blob_bytes: usize, retained_bytes: usize, entries: usize) -> AttachmentCatalog {
        AttachmentCatalog::with_limits(CatalogLimits {
            blob_bytes,
            retained_bytes,
            entries,
        })
    }

    fn rich_blocks() -> Vec<ContentBlock> {
        vec![
            ContentBlock::Text(TextContent {
                text: "Visible text".to_string(),
                text_signature: Some("text-signature".to_string()),
            }),
            ContentBlock::Thinking(ThinkingContent {
                thinking: "Reasoning".to_string(),
                thinking_signature: Some("thinking-signature".to_string()),
            }),
            ContentBlock::RedactedThinking(RedactedThinkingContent {
                data: "b3BhcXVl".to_string(),
            }),
            ContentBlock::Image(ImageContent {
                data: "YWJj".to_string(),
                mime_type: "image/png".to_string(),
            }),
            ContentBlock::Media(MediaContent {
                data: "ZGU".to_string(),
                mime_type: "audio/wav".to_string(),
                name: Some("voice.wav".to_string()),
            }),
            ContentBlock::Media(MediaContent {
                data: "Zmdo".to_string(),
                mime_type: "video/mp4".to_string(),
                name: None,
            }),
            ContentBlock::ToolCall(ToolCall {
                id: "call-1".to_string(),
                name: "inspect".to_string(),
                arguments: json!({
                    "type": "image",
                    "data": "b3BhcXVl",
                    "mimeType": "image/png"
                }),
                thought_signature: Some("tool-signature".to_string()),
            }),
        ]
    }

    fn rich_assistant() -> AssistantMessage {
        AssistantMessage {
            content: rich_blocks(),
            api: "test-api".to_string(),
            provider: "test-provider".to_string(),
            model: "test-model".to_string(),
            usage: Usage {
                input: 11,
                output: 7,
                cache_read: 3,
                cache_write: 2,
                total_tokens: 23,
                cost: Cost {
                    input: 0.1,
                    output: 0.2,
                    cache_read: 0.03,
                    cache_write: 0.04,
                    total: 0.37,
                },
            },
            stop_reason: StopReason::Refusal,
            stop_details: Some(StopDetails {
                kind: "refusal".to_string(),
                category: None,
                explanation: Some("Provider explanation".to_string()),
            }),
            error_message: Some("Provider diagnostic".to_string()),
            timestamp: 123,
        }
    }

    // Only native attachment slots may differ from the ordinary serializer.
    // Reconstructing those slots must recover the complete original value.
    fn restore_attachment_data(
        projected: &mut Value,
        original: &Value,
        catalog: &AttachmentCatalog,
    ) -> usize {
        let Some(original_blocks) = original.get("content").and_then(Value::as_array) else {
            assert_eq!(&*projected, original);
            return 0;
        };
        let projected_blocks = projected["content"].as_array_mut().unwrap();
        assert_eq!(projected_blocks.len(), original_blocks.len());
        let mut references = 0;
        for (projected_block, original_block) in
            projected_blocks.iter_mut().zip(original_blocks)
        {
            if matches!(
                original_block["type"].as_str(),
                Some("image" | "media")
            ) {
                let reference = &projected_block["data"];
                let attachment_id = reference["attachmentId"].as_str().unwrap();
                let chunk = catalog.read(attachment_id, 0, MAX_CHUNK_BYTES).unwrap();
                let original_data = original_block["data"].as_str().unwrap();
                let expected = STANDARD
                    .decode(original_data)
                    .or_else(|_| STANDARD_NO_PAD.decode(original_data))
                    .unwrap();
                assert_eq!(STANDARD.decode(&chunk.data).unwrap(), expected);
                assert_eq!(reference["sizeBytes"], expected.len());
                assert_eq!(reference["$piBlob"], chunk.digest);
                assert!(matches!(
                    reference["encoding"].as_str(),
                    Some("base64" | "base64NoPad")
                ));
                assert!(chunk.eof);
                projected_block["data"] = original_block["data"].clone();
                references += 1;
            }
            assert_eq!(&*projected_block, original_block);
        }
        assert_eq!(&*projected, original);
        references
    }

    #[test]
    fn decoded_digest_and_bounded_chunks_reconstruct_exact_bytes() {
        let catalog = AttachmentCatalog::default();
        let reference = catalog.register("YWJj").unwrap();
        assert_eq!(
            reference.digest,
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            serde_json::to_value(&reference).unwrap(),
            json!({
                "attachmentId": reference.attachment_id,
                "$piBlob": reference.digest,
                "sizeBytes": 3,
                "encoding": "base64"
            })
        );

        let first = catalog.read(&reference.attachment_id, 0, 2).unwrap();
        assert_eq!(
            serde_json::to_value(&first).unwrap(),
            json!({
                "attachmentId": reference.attachment_id,
                "$piBlob": reference.digest,
                "offset": 0,
                "nextOffset": 2,
                "bytesReturned": 2,
                "sizeBytes": 3,
                "data": "YWI=",
                "eof": false
            })
        );
        let second = catalog
            .read(&reference.attachment_id, first.next_offset, 2)
            .unwrap();
        assert_eq!(second.data, "Yw==");
        assert_eq!(second.bytes_returned, 1);
        assert_eq!(second.next_offset, 3);
        assert!(second.eof);
        let mut reconstructed = STANDARD.decode(&first.data).unwrap();
        reconstructed.extend(STANDARD.decode(&second.data).unwrap());
        assert_eq!(reconstructed, b"abc");
        let end = catalog.read(&reference.attachment_id, 3, 2).unwrap();
        assert_eq!(end.bytes_returned, 0);
        assert_eq!(end.next_offset, 3);
        assert!(end.data.is_empty());
        assert!(end.eof);
    }

    #[test]
    fn canonical_padding_variants_deduplicate_under_full_quota() {
        let catalog = small_catalog(8, 1, 1);
        let padded = catalog.register("eA==").unwrap();
        let unpadded = catalog.register("eA").unwrap();
        assert_eq!(padded.attachment_id, unpadded.attachment_id);
        assert_eq!(padded.digest, unpadded.digest);
        assert_eq!(padded.encoding, Encoding::Standard);
        assert_eq!(unpadded.encoding, Encoding::NoPad);
        assert_eq!(catalog.register("eA=="), Some(padded.clone()));
        assert_eq!(catalog.register("eA"), Some(unpadded));
        let state = catalog.state.lock().unwrap();
        assert_eq!(state.blobs.len(), 1);
        assert_eq!(state.encoded.len(), 2);
        assert_eq!(state.retained_bytes, 1);
        drop(state);
        assert_eq!(
            STANDARD
                .decode(catalog.read(&padded.attachment_id, 0, 1).unwrap().data)
                .unwrap(),
            b"x"
        );
    }

    #[test]
    fn admission_failures_preserve_inline_payload_and_issued_references() {
        let catalog = small_catalog(4, 4, 4);
        let first = catalog.register("YWJjZA==").unwrap();
        let fallback = vec![ContentBlock::Image(ImageContent {
            data: "ZWZnaA==".to_string(),
            mime_type: "image/png".to_string(),
        })];
        let native = serde_json::to_value(&fallback).unwrap();
        assert_eq!(
            serde_json::to_value(ContentBlocks::new(&fallback, &catalog)).unwrap(),
            native
        );
        assert_eq!(catalog.register("ZWZnaA=="), None);
        assert_eq!(
            STANDARD
                .decode(catalog.read(&first.attachment_id, 0, 4).unwrap().data)
                .unwrap(),
            b"abcd"
        );
        assert_eq!(catalog.register("YWJjZGU="), None);
        assert_eq!(catalog.state.lock().unwrap().retained_bytes, 4);

        let entry_limited = small_catalog(4, 100, 1);
        let retained = entry_limited.register("eA==").unwrap();
        assert_eq!(entry_limited.register("eQ=="), None);
        assert!(entry_limited.read(&retained.attachment_id, 0, 1).is_ok());

        let legacy = small_catalog(16, 32, 4);
        for data in ["", "not base64!", "YR==", "YR", "YQ=", "YQ==\n", "_w=="] {
            assert_eq!(legacy.register(data), None, "{data:?}");
            let blocks = vec![ContentBlock::Media(MediaContent {
                data: data.to_string(),
                mime_type: "audio/wav".to_string(),
                name: Some("legacy.wav".to_string()),
            })];
            assert_eq!(
                serde_json::to_value(ContentBlocks::new(&blocks, &legacy)).unwrap(),
                serde_json::to_value(&blocks).unwrap()
            );
        }
        assert!(legacy.state.lock().unwrap().blobs.is_empty());
    }

    #[test]
    fn clearing_shared_catalog_expires_ids_even_when_bytes_return() {
        let catalog = AttachmentCatalog::default();
        let old = catalog.register("YWJj").unwrap();
        catalog.clone().clear();
        assert_eq!(
            catalog.read(&old.attachment_id, 0, 1).unwrap_err(),
            AttachmentReadError::UnknownId
        );
        let new = catalog.register("YWJj").unwrap();
        assert_eq!(old.digest, new.digest);
        assert_ne!(old.attachment_id, new.attachment_id);
        assert_eq!(
            catalog.read(&old.attachment_id, 0, 1).unwrap_err(),
            AttachmentReadError::UnknownId
        );
        assert!(catalog.read(&new.attachment_id, 0, 1).is_ok());
    }

    #[test]
    fn registration_started_before_clear_cannot_repopulate_next_generation() {
        let catalog = AttachmentCatalog::default();
        let generation = catalog.state.lock().unwrap().generation;
        let encoded_key = Sha256::digest(b"YWJj").into();
        catalog.clear();
        assert_eq!(
            catalog.retain(
                generation,
                encoded_key,
                Encoding::Standard,
                b"abc".to_vec()
            ),
            None
        );
        let state = catalog.state.lock().unwrap();
        assert!(state.blobs.is_empty());
        assert!(state.encoded.is_empty());
        assert_eq!(state.retained_bytes, 0);
    }

    #[test]
    fn retrieval_rejects_invalid_bounds_and_never_uses_paths_or_bare_digests() {
        let catalog = AttachmentCatalog::default();
        let reference = catalog.register("cHJpdmF0ZS1wYXlsb2Fk").unwrap();
        for id in ["", "../../private-payload", "rpc:\nprivate-payload"] {
            let error = catalog.read(id, 0, 1).unwrap_err();
            assert_eq!(error, AttachmentReadError::InvalidId);
            assert!(!error.to_string().contains("private-payload"));
        }
        assert_eq!(
            catalog
                .read(&"a".repeat(MAX_ATTACHMENT_ID_BYTES + 1), 0, 1)
                .unwrap_err(),
            AttachmentReadError::InvalidId
        );
        assert_eq!(
            catalog.read("rpc:unknown", 0, 1).unwrap_err(),
            AttachmentReadError::UnknownId
        );
        assert_eq!(
            catalog.read(&reference.digest, 0, 1).unwrap_err(),
            AttachmentReadError::UnknownId
        );
        assert_eq!(
            catalog
                .read(&reference.attachment_id, usize::MAX, 1)
                .unwrap_err(),
            AttachmentReadError::InvalidOffset
        );
        for max_bytes in [0, MAX_CHUNK_BYTES + 1] {
            assert_eq!(
                catalog
                    .read(&reference.attachment_id, 0, max_bytes)
                    .unwrap_err(),
                AttachmentReadError::InvalidLength
            );
        }
        let chunk = catalog
            .read(&reference.attachment_id, 0, MAX_CHUNK_BYTES)
            .unwrap();
        let diagnostic = format!("{catalog:?} {chunk:?}");
        assert!(!diagnostic.contains("private-payload"));
        assert!(!diagnostic.contains(&chunk.data));
    }

    #[test]
    fn model_projection_preserves_every_field_and_opaque_data() {
        let messages = [
            Message::User(UserMessage {
                content: UserContent::Blocks(rich_blocks()),
                timestamp: 1,
            }),
            Message::User(UserMessage {
                content: UserContent::Text("Plain user text".to_string()),
                timestamp: 2,
            }),
            Message::assistant(rich_assistant()),
            Message::assistant(AssistantMessage::default()),
            Message::tool_result(ToolResultMessage {
                tool_call_id: "call-1".to_string(),
                tool_name: "inspect".to_string(),
                content: rich_blocks(),
                details: Some(json!({"type": "image", "data": "b3BhcXVl"})),
                is_error: true,
                timestamp: 3,
            }),
            Message::tool_result(ToolResultMessage {
                tool_call_id: "call-2".to_string(),
                tool_name: "inspect".to_string(),
                content: vec![ContentBlock::Text(TextContent::new("Done"))],
                details: None,
                is_error: false,
                timestamp: 4,
            }),
            Message::Custom(CustomMessage {
                content: "Custom message".to_string(),
                custom_type: "test".to_string(),
                display: false,
                details: Some(json!({"type": "media", "data": "b3BhcXVl"})),
                timestamp: 5,
            }),
        ];
        let catalog = AttachmentCatalog::default();
        let mut references = 0;
        for message in &messages {
            let original = serde_json::to_value(message).unwrap();
            let mut projected =
                serde_json::to_value(ModelMessageProjection::new(message, &catalog)).unwrap();
            references += restore_attachment_data(&mut projected, &original, &catalog);
            assert_eq!(serde_json::to_value(message).unwrap(), original);
        }
        assert_eq!(references, 9);
        // Opaque thinking, tool arguments and custom details add no blobs.
        assert_eq!(catalog.state.lock().unwrap().blobs.len(), 3);
    }

    #[test]
    fn assistant_event_projection_has_no_role_and_preserves_terminal_details() {
        let catalog = AttachmentCatalog::default();
        for message in [rich_assistant(), AssistantMessage::default()] {
            let original = serde_json::to_value(&message).unwrap();
            let mut projected =
                serde_json::to_value(AssistantMessageProjection::new(&message, &catalog)).unwrap();
            assert!(projected.get("role").is_none());
            restore_attachment_data(&mut projected, &original, &catalog);
            assert_eq!(serde_json::to_value(&message).unwrap(), original);
        }
    }

    #[test]
    fn session_projection_preserves_optional_fields_and_all_message_variants() {
        let messages = [
            SessionMessage::User {
                content: UserContent::Blocks(rich_blocks()),
                timestamp: None,
            },
            SessionMessage::User {
                content: UserContent::Text("Plain user text".to_string()),
                timestamp: Some(1),
            },
            SessionMessage::Assistant {
                message: rich_assistant(),
            },
            SessionMessage::ToolResult {
                tool_call_id: "call-1".to_string(),
                tool_name: "inspect".to_string(),
                content: rich_blocks(),
                details: Some(json!({"type": "image", "data": "b3BhcXVl"})),
                is_error: true,
                timestamp: Some(2),
            },
            SessionMessage::ToolResult {
                tool_call_id: "call-2".to_string(),
                tool_name: "inspect".to_string(),
                content: vec![],
                details: None,
                is_error: false,
                timestamp: None,
            },
            SessionMessage::Custom {
                custom_type: "test".to_string(),
                content: "Custom message".to_string(),
                display: true,
                details: Some(json!({"type": "media", "data": "b3BhcXVl"})),
                timestamp: None,
            },
            SessionMessage::BashExecution {
                command: "example command".to_string(),
                output: "Example output".to_string(),
                exit_code: 1,
                cancelled: Some(false),
                truncated: Some(true),
                full_output_path: Some("/example/output".to_string()),
                timestamp: Some(3),
                extra: HashMap::from([(
                    "legacy".to_string(),
                    json!({"type": "image", "data": "b3BhcXVl"}),
                )]),
            },
            SessionMessage::BranchSummary {
                summary: "Branch summary".to_string(),
                from_id: "entry-1".to_string(),
            },
            SessionMessage::CompactionSummary {
                summary: "Compaction summary".to_string(),
                tokens_before: 1234,
            },
        ];
        let catalog = AttachmentCatalog::default();
        let mut references = 0;
        for message in &messages {
            let original = serde_json::to_value(message).unwrap();
            let mut projected =
                serde_json::to_value(SessionMessageProjection::new(message, &catalog)).unwrap();
            references += restore_attachment_data(&mut projected, &original, &catalog);
            assert_eq!(serde_json::to_value(message).unwrap(), original);
        }
        assert_eq!(references, 9);
        assert_eq!(catalog.state.lock().unwrap().blobs.len(), 3);
    }
}
