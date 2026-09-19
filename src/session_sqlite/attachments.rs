//! Transactional, content-addressed attachments for SQLite sessions.
//!
//! Only native image/media blocks in message content are transformed. Tool
//! arguments, custom entries, opaque provider items, and tool details are not
//! searched recursively. References live in this backend only: providers,
//! forks, JSONL exports, and the SDK still receive complete native messages.
//!
//! A data reference is an object, not a magic string. Older readers therefore
//! reject it rather than forwarding a reference as if it were base64 media.
//! Blob rows are immutable and are never garbage-collected during a save.

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use std::io::Write;
use std::sync::Arc;

use super::{
    Error, MAX_SQLITE_JSON_BYTES, Result, SessionEntry, SqliteConnection, SqliteValue,
    malformed_json_error, map_sqlite_result, parse_sqlite_json, validate_sqlite_json_for_write,
};

const INLINE_BYTES: usize = 64 * 1024;
const MAX_BLOB_BYTES: usize = 64 * 1024 * 1024;
const MAX_REFERENCES: usize = 4096;
const INIT_BLOBS: &str = "CREATE TABLE IF NOT EXISTS pi_session_blobs (\
    digest TEXT PRIMARY KEY NOT NULL, \
    byte_length INTEGER NOT NULL, \
    data BLOB NOT NULL)";

fn blob_error(reason: &'static str) -> Error {
    Error::session(format!("PI_SESSION_ATTACHMENT_INVALID: {reason}"))
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
        let full = bytes
            .checked_div(3)
            .and_then(|groups| groups.checked_mul(4))
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

#[derive(Debug, Deserialize, Serialize)]
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
    value
        .get_mut("message")?
        .get_mut("content")?
        .as_array_mut()
}

fn is_attachment(block: &Value) -> bool {
    matches!(
        block.get("type").and_then(Value::as_str),
        Some("image" | "media")
    )
}

/// Invalid or noncanonical historical payloads are preserved inline, not
/// silently repaired. Exact base64 spelling matters to immutable entry IDs.
fn prepare(data: &str) -> Result<Option<(BlobRef, Vec<u8>)>> {
    if data.len() <= INLINE_BYTES
        || data.len() > Encoding::Standard.encoded_len(MAX_BLOB_BYTES)?
    {
        return Ok(None);
    }
    let (encoding, bytes) = if let Ok(bytes) = STANDARD.decode(data) {
        (Encoding::Standard, bytes)
    } else if let Ok(bytes) = STANDARD_NO_PAD.decode(data) {
        (Encoding::NoPad, bytes)
    } else {
        return Ok(None);
    };
    if bytes.len() <= INLINE_BYTES || bytes.len() > MAX_BLOB_BYTES {
        return Ok(None);
    }
    // Do not let a future decoder relaxation normalize an existing entry.
    if encoding.encode(&bytes) != data {
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

/// Metadata is checked before requesting a BLOB column from the driver. The
/// payload is checked again afterwards; no SQL constraint is trusted alone.
fn blob_exists(conn: &SqliteConnection, reference: &BlobRef) -> Result<bool> {
    let rows = map_sqlite_result(conn.query_sync(
        "SELECT byte_length,length(data),typeof(data) FROM pi_session_blobs \
         WHERE digest = ?1 LIMIT 2",
        &[SqliteValue::from(reference.digest.clone())],
    ))?;
    if rows.is_empty() {
        return Ok(false);
    }
    if rows.len() != 1 {
        return Err(blob_error("duplicate attachment rows"));
    }
    let expected = i64::try_from(reference.size_bytes)
        .map_err(|_| blob_error("attachment byte count exceeds SQLite INTEGER"))?;
    let row = &rows[0];
    if !matches!(row.get(0), Some(SqliteValue::Integer(size)) if *size == expected)
        || !matches!(row.get(1), Some(SqliteValue::Integer(size)) if *size == expected)
        || !matches!(row.get(2), Some(SqliteValue::Text(kind)) if kind.as_str() == "blob")
    {
        return Err(blob_error(
            "attachment metadata or storage type does not match",
        ));
    }
    Ok(true)
}

fn load_blob(conn: &SqliteConnection, reference: &BlobRef) -> Result<Arc<[u8]>> {
    if !blob_exists(conn, reference)? {
        return Err(blob_error("referenced attachment is missing"));
    }
    let size = i64::try_from(reference.size_bytes)
        .map_err(|_| blob_error("attachment byte count exceeds SQLite INTEGER"))?;
    let rows = map_sqlite_result(conn.query_sync(
        "SELECT data FROM pi_session_blobs WHERE digest = ?1 AND byte_length = ?2 \
         AND length(data) = ?2 AND typeof(data) = 'blob' LIMIT 2",
        &[
            SqliteValue::from(reference.digest.clone()),
            SqliteValue::from(size),
        ],
    ))?;
    if rows.len() != 1 {
        return Err(blob_error("attachment changed while being read"));
    }
    let Some(SqliteValue::Blob(bytes)) = rows[0].get(0) else {
        return Err(blob_error("attachment payload is not a BLOB"));
    };
    if bytes.len() != reference.size_bytes || digest(bytes) != reference.digest {
        return Err(blob_error("attachment digest or byte count does not match"));
    }
    Ok(Arc::clone(bytes))
}

/// Owned by `insert_entry_jsons`, within its caller's existing transaction.
pub(super) struct EntryEncoder<'a> {
    conn: &'a SqliteConnection,
    initialized: bool,
}

impl<'a> EntryEncoder<'a> {
    pub(super) const fn new(conn: &'a SqliteConnection) -> Self {
        Self {
            conn,
            initialized: false,
        }
    }

    pub(super) fn encode(&mut self, json: &str) -> Result<String> {
        validate_sqlite_json_for_write("session entry", json)?;
        let mut value: Value = parse_sqlite_json("session entry", json)?;
        let Some(blocks) = message_blocks(&mut value) else {
            return Ok(json.to_string());
        };
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
            self.store(&reference, bytes)?;
            *data = serde_json::to_value(reference)?;
        }
        if count == 0 {
            Ok(json.to_string())
        } else {
            let encoded = serde_json::to_string(&value)?;
            validate_sqlite_json_for_write("stored session entry", &encoded)?;
            Ok(encoded)
        }
    }

    fn store(&mut self, reference: &BlobRef, bytes: Vec<u8>) -> Result<()> {
        if !self.initialized {
            map_sqlite_result(self.conn.execute_raw(INIT_BLOBS))?;
            self.initialized = true;
        }
        if blob_exists(self.conn, reference)? {
            // Never overwrite an existing row to "heal" corruption. Comparing
            // bytes also fails closed on a hypothetical digest collision.
            if load_blob(self.conn, reference)?.as_ref() != bytes.as_slice() {
                return Err(blob_error("existing attachment has different content"));
            }
            return Ok(());
        }
        let size = i64::try_from(reference.size_bytes)
            .map_err(|_| blob_error("attachment byte count exceeds SQLite INTEGER"))?;
        map_sqlite_result(self.conn.execute_sync(
            "INSERT INTO pi_session_blobs (digest,byte_length,data) VALUES (?1,?2,?3)",
            &[
                SqliteValue::from(reference.digest.clone()),
                SqliteValue::from(size),
                SqliteValue::from(bytes),
            ],
        ))?;
        Ok(())
    }
}

struct JsonSize(usize);

impl Write for JsonSize {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len())
            .filter(|size| *size <= MAX_SQLITE_JSON_BYTES)
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

/// Validate the ENTIRE expansion before reading any blob. Repeating a small
/// reference cannot multiply a bounded stored JSON row into unbounded memory.
fn hydration_plan(value: &mut Value) -> Result<Vec<(usize, BlobRef)>> {
    let mut expanded = json_size(value)?;
    let mut plan = Vec::new();
    if let Some(blocks) = message_blocks(value) {
        for (index, block) in blocks.iter().enumerate() {
            if !is_attachment(block) {
                continue;
            }
            let Some(data) = block.get("data").filter(|data| !data.is_string()) else {
                continue;
            };
            if plan.len() >= MAX_REFERENCES {
                return Err(blob_error("too many attachment references in one entry"));
            }
            let reference = BlobRef::parse(data)?;
            let encoded_bytes = reference.encoding.encoded_len(reference.size_bytes)?;
            expanded = expanded
                .checked_sub(json_size(data)?)
                .and_then(|size| size.checked_add(encoded_bytes))
                // Base64 needs only the two JSON string quotes, no escapes.
                .and_then(|size| size.checked_add(2))
                .filter(|size| *size <= MAX_SQLITE_JSON_BYTES)
                .ok_or_else(|| blob_error("hydrated entry exceeds JSON limit"))?;
            plan.push((index, reference));
        }
    }
    Ok(plan)
}

pub(super) fn decode_entry(conn: &SqliteConnection, json: &str) -> Result<SessionEntry> {
    let mut value: Value = parse_sqlite_json("session entry", json)?;
    let plan = hydration_plan(&mut value)?;
    if plan.is_empty() {
        // Preserve the existing typed reader's validation for inline/legacy
        // rows, including its handling of duplicate native message fields.
        return parse_sqlite_json("session entry", json);
    }
    if let Some(blocks) = message_blocks(&mut value) {
        for (index, reference) in plan {
            let bytes = load_blob(conn, &reference)?;
            blocks[index]["data"] = Value::String(reference.encoding.encode(&bytes));
        }
    }
    serde_json::from_value(value)
        .map_err(|error| malformed_json_error("session entry", json, &error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        AssistantMessage, ContentBlock, ImageContent, MediaContent, Message, TextContent,
        ToolResultMessage, UserContent,
    };
    use crate::session::{CustomEntry, EntryBase, MessageEntry, SessionHeader, SessionMessage};
    use crate::session_sqlite::{
        INIT_SQL, append_entries, insert_entry_jsons, load_session, run_on_sqlite_thread,
        save_session,
    };
    use serde_json::json;

    fn base(id: &str) -> EntryBase {
        EntryBase {
            id: Some(id.to_string()),
            parent_id: None,
            timestamp: "2026-09-19T00:00:00.000Z".to_string(),
        }
    }

    fn image(data: String) -> ContentBlock {
        ContentBlock::Image(ImageContent {
            data,
            mime_type: "image/png".to_string(),
        })
    }

    fn entry(id: &str, blocks: Vec<ContentBlock>) -> SessionEntry {
        SessionEntry::Message(MessageEntry {
            base: base(id),
            message: SessionMessage::User {
                content: UserContent::Blocks(blocks),
                timestamp: None,
            },
        })
    }

    fn encoded(entry: &SessionEntry) -> String {
        serde_json::to_string(entry).expect("encode native entry")
    }

    fn fixture(id: &str, byte: u8) -> SessionEntry {
        entry(id, vec![image(STANDARD.encode(vec![byte; INLINE_BYTES + 11]))])
    }

    fn with_database<T: Send>(f: impl FnOnce(&SqliteConnection) -> Result<T> + Send) -> T {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("attachments.sqlite");
        run_on_sqlite_thread(|| {
            let conn = map_sqlite_result(SqliteConnection::open_read_write(&path))?;
            map_sqlite_result(conn.execute_raw(INIT_SQL))?;
            map_sqlite_result(conn.execute_raw("BEGIN IMMEDIATE"))?;
            let value = f(&conn)?;
            map_sqlite_result(conn.execute_raw("COMMIT"))?;
            map_sqlite_result(conn.close())?;
            Ok(value)
        })
        .expect("SQLite attachment fixture")
    }

    fn count(conn: &SqliteConnection) -> usize {
        conn.query_sync("SELECT digest FROM pi_session_blobs", &[])
            .expect("blob rows")
            .len()
    }

    #[test]
    fn native_mixed_media_round_trips_without_putting_payloads_in_entry_rows() {
        let bytes = vec![7; INLINE_BYTES + 11];
        let data = STANDARD.encode(&bytes);
        let original = entry(
            "mixed",
            vec![
                ContentBlock::Text(TextContent::new("before")),
                image(data.clone()),
                ContentBlock::Text(TextContent::new("between")),
                ContentBlock::Media(MediaContent {
                    data: data.clone(),
                    mime_type: "audio/wav".to_string(),
                    name: Some("音声.wav".to_string()),
                }),
            ],
        );
        with_database(|conn| {
            let json = encoded(&original);
            let stored = EntryEncoder::new(conn).encode(&json)?;
            assert!(stored.len() < 2048);
            assert!(!stored.contains(&data));
            assert_eq!(count(conn), 1, "identical bytes deduplicate across MIME types");
            let restored = decode_entry(conn, &stored)?;
            assert_eq!(encoded(&restored), json);
            let wire = serde_json::to_value(restored)?;
            assert_eq!(wire["message"]["content"][1]["data"], data);
            assert_eq!(wire["message"]["content"][3]["name"], "音声.wav");
            Ok(())
        });
    }

    #[test]
    fn assistant_and_tool_result_attachments_use_the_same_storage_path() {
        let data = STANDARD.encode(vec![8; INLINE_BYTES + 13]);
        let details = json!({
            "type": "image", "data": data, "mimeType": "image/png",
            "note": "opaque tool details must not become a storage reference"
        });
        let messages = [
            Message::assistant(AssistantMessage {
                content: vec![image(data.clone())],
                ..AssistantMessage::default()
            }),
            Message::tool_result(ToolResultMessage {
                tool_call_id: "call-1".to_string(),
                tool_name: "inspect_image".to_string(),
                content: vec![
                    ContentBlock::Text(TextContent::new("tool attachment")),
                    image(data.clone()),
                ],
                details: Some(details.clone()),
                is_error: false,
                timestamp: 1,
            }),
        ];
        with_database(|conn| {
            let mut encoder = EntryEncoder::new(conn);
            for (index, message) in messages.into_iter().enumerate() {
                let original = SessionEntry::Message(MessageEntry {
                    base: base(&format!("native-{index}")),
                    message: SessionMessage::from(message),
                });
                let json = encoded(&original);
                let stored = encoder.encode(&json)?;
                let stored_value: Value = serde_json::from_str(&stored)?;
                let blocks = stored_value["message"]["content"]
                    .as_array()
                    .expect("native content array");
                assert!(blocks.last().expect("image")["data"].is_object());
                if index == 1 {
                    assert_eq!(stored_value["message"]["details"], details);
                }
                assert_eq!(encoded(&decode_entry(conn, &stored)?), json);
            }
            assert_eq!(count(conn), 1);
            Ok(())
        });
    }

    #[test]
    fn raw_content_dedup_preserves_padded_and_unpadded_spelling() {
        for length in [INLINE_BYTES + 1, INLINE_BYTES + 2, INLINE_BYTES + 3] {
            let bytes = vec![3; length];
            let padded = encoded(&entry("padded", vec![image(STANDARD.encode(&bytes))]));
            let unpadded = encoded(&entry("unpadded", vec![image(STANDARD_NO_PAD.encode(&bytes))]));
            with_database(|conn| {
                let mut encoder = EntryEncoder::new(conn);
                let first = encoder.encode(&padded)?;
                let second = encoder.encode(&unpadded)?;
                assert_eq!(count(conn), 1);
                assert_eq!(encoded(&decode_entry(conn, &first)?), padded);
                assert_eq!(encoded(&decode_entry(conn, &second)?), unpadded);
                Ok(())
            });
        }
    }

    #[test]
    fn small_and_opaque_inline_data_are_unchanged_and_need_no_blob_table() {
        for data in [
            String::new(),
            STANDARD.encode(vec![0; INLINE_BYTES]),
            "not-base64!".repeat(INLINE_BYTES / 4),
            "sha256:not-a-storage-reference".to_string(),
        ] {
            let json = encoded(&entry("inline", vec![image(data)]));
            with_database(|conn| {
                assert_eq!(EntryEncoder::new(conn).encode(&json)?, json);
                assert_eq!(encoded(&decode_entry(conn, &json)?), json);
                let tables = conn.query_sync(
                    "SELECT name FROM sqlite_master WHERE name = 'pi_session_blobs'",
                    &[],
                );
                assert!(tables.expect("schema lookup").is_empty());
                Ok(())
            });
        }
    }

    #[test]
    fn only_message_content_is_externalized_not_custom_data_or_tool_arguments() {
        let media = json!({
            "type": "image",
            "data": STANDARD.encode(vec![1; INLINE_BYTES + 1]),
            "mimeType": "image/png"
        });
        let custom = SessionEntry::Custom(CustomEntry {
            base: base("custom"),
            custom_type: "image".to_string(),
            data: Some(media.clone()),
        });
        with_database(|conn| {
            let json = encoded(&custom);
            assert_eq!(EntryEncoder::new(conn).encode(&json)?, json);
            assert_eq!(encoded(&decode_entry(conn, &json)?), json);
            let raw = json!({
                "type":"message",
                "message":{
                    "role":"assistant",
                    "content":[{"type":"toolCall","id":"call","name":"echo","arguments":media}],
                    "details":{"content":[media]}
                }
            })
            .to_string();
            assert_eq!(EntryEncoder::new(conn).encode(&raw)?, raw);
            Ok(())
        });
    }

    #[test]
    fn references_in_arbitrary_custom_json_do_not_trigger_storage_reads() {
        let custom = SessionEntry::Custom(CustomEntry {
            base: base("opaque"),
            custom_type: "message".to_string(),
            data: Some(json!({
                "type":"image",
                "data":{"$piBlob":"sha256:missing","sizeBytes":1,"encoding":"base64"}
            })),
        });
        with_database(|conn| {
            let json = encoded(&custom);
            assert_eq!(encoded(&decode_entry(conn, &json)?), json);
            Ok(())
        });
    }

    #[test]
    fn missing_blob_is_an_error_not_an_empty_attachment() {
        with_database(|conn| {
            let stored = EntryEncoder::new(conn).encode(&encoded(&fixture("missing", 9)))?;
            let mut value: Value = serde_json::from_str(&stored)?;
            value["message"]["content"][0]["data"]["$piBlob"] =
                Value::String(format!("sha256:{}", "0".repeat(64)));
            let error = decode_entry(conn, &value.to_string()).expect_err("missing blob");
            assert!(error.to_string().contains("referenced attachment is missing"));
            Ok(())
        });
    }

    #[test]
    fn same_length_corruption_is_rejected_and_never_overwritten_on_reuse() {
        with_database(|conn| {
            let original = encoded(&fixture("damaged", 4));
            let stored = EntryEncoder::new(conn).encode(&original)?;
            let corrupt = vec![5; INLINE_BYTES + 11];
            map_sqlite_result(conn.execute_sync(
                "UPDATE pi_session_blobs SET data = ?1",
                &[SqliteValue::from(corrupt.clone())],
            ))?;
            let error = decode_entry(conn, &stored).expect_err("corrupt bytes");
            assert!(error.to_string().contains("digest or byte count"));
            EntryEncoder::new(conn)
                .encode(&original)
                .expect_err("do not heal corruption");
            let rows = map_sqlite_result(
                conn.query_sync("SELECT data FROM pi_session_blobs", &[]),
            )?;
            assert!(
                matches!(rows[0].get(0), Some(SqliteValue::Blob(bytes)) if bytes.as_ref() == corrupt.as_slice())
            );
            Ok(())
        });
    }

    #[test]
    fn stored_blob_lengths_and_sql_types_are_not_trusted() {
        for mutation in [
            "UPDATE pi_session_blobs SET byte_length = -1",
            "UPDATE pi_session_blobs SET byte_length = byte_length + 1",
            "UPDATE pi_session_blobs SET data = 'PRIVATE_PAYLOAD_DO_NOT_ECHO'",
        ] {
            with_database(|conn| {
                let stored = EntryEncoder::new(conn).encode(&encoded(&fixture("types", 1)))?;
                map_sqlite_result(conn.execute_raw(mutation))?;
                let error = decode_entry(conn, &stored).expect_err("tampered blob row");
                assert!(error.to_string().contains("metadata or storage type"));
                assert!(!error.to_string().contains("PRIVATE_PAYLOAD"));
                Ok(())
            });
        }
    }

    #[test]
    fn malformed_reference_metadata_is_rejected_without_echoing_values() {
        let valid = json!({
            "$piBlob": format!("sha256:{}", "a".repeat(64)),
            "sizeBytes": 99,
            "encoding": "base64"
        });
        let mut cases = Vec::new();
        for (key, value) in [
            ("$piBlob", json!("file:///PRIVATE_PATH")),
            ("$piBlob", json!(format!("sha256:{}", "A".repeat(64)))),
            ("sizeBytes", json!(0)),
            ("sizeBytes", json!(MAX_BLOB_BYTES + 1)),
            ("sizeBytes", json!(-1)),
            ("encoding", json!("PRIVATE_UNSUPPORTED_ENCODING")),
            ("PRIVATE_UNKNOWN_FIELD", json!("PRIVATE_VALUE")),
        ] {
            let mut altered = valid.clone();
            altered[key] = value;
            cases.push(altered);
        }
        cases.extend([Value::Null, json!([]), json!("magic-string")]);
        for value in cases {
            let error = BlobRef::parse(&value).expect_err("invalid reference");
            assert!(!error.to_string().contains("PRIVATE"));
        }
    }

    #[test]
    fn aggregate_hydration_budget_is_checked_before_any_blob_lookup() {
        let reference = json!({
            "$piBlob":format!("sha256:{}", "a".repeat(64)),
            "sizeBytes":MAX_BLOB_BYTES,
            "encoding":"base64"
        });
        let value = json!({
            "type":"message",
            "message":{"role":"user","content":[
                {"type":"image","data":reference,"mimeType":"image/png"},
                {"type":"media","data":reference,"mimeType":"video/mp4"}
            ]}
        });
        with_database(|conn| {
            // There is no blob table: an attempted lookup would fail with a
            // table error instead of the required pre-allocation budget error.
            let error = decode_entry(conn, &value.to_string()).expect_err("expansion bomb");
            assert!(error.to_string().contains("hydrated entry exceeds JSON limit"));
            Ok(())
        });
    }

    #[test]
    fn byte_length_accounting_matches_both_base64_engines() {
        for length in 0..300 {
            let bytes = vec![0; length];
            for encoding in [Encoding::Standard, Encoding::NoPad] {
                assert_eq!(
                    encoding.encoded_len(length).expect("length"),
                    encoding.encode(&bytes).len()
                );
            }
        }
    }

    #[test]
    fn reference_count_is_bounded_even_for_tiny_valid_references() {
        let block = json!({
            "type": "image", "mimeType": "image/png",
            "data": {
                "$piBlob": format!("sha256:{}", "a".repeat(64)),
                "sizeBytes": 1,
                "encoding": "base64"
            }
        });
        let mut value = json!({
            "type": "message", "message": {"content": vec![block; MAX_REFERENCES + 1]}
        });
        assert!(
            hydration_plan(&mut value)
                .expect_err("reference count")
                .to_string()
                .contains("too many attachment references")
        );
    }

    #[test]
    fn entry_and_blob_changes_follow_the_callers_transaction_rollback() {
        with_database(|conn| {
            insert_entry_jsons(conn, &[encoded(&fixture("first", 1))], 0)?;
            assert_eq!(count(conn), 1);
            map_sqlite_result(conn.execute_raw("SAVEPOINT pending_append"))?;
            insert_entry_jsons(conn, &[encoded(&fixture("second", 2)), "{".to_string()], 1)
                .expect_err("late entry serialization failure");
            assert_eq!(count(conn), 2, "new blob was staged inside the savepoint");
            map_sqlite_result(
                conn.execute_raw("ROLLBACK TO pending_append; RELEASE pending_append"),
            )?;
            assert_eq!(count(conn), 1, "no orphaned blob after rollback");
            assert_eq!(super::super::read_all_entries(conn)?.len(), 1);
            Ok(())
        });
    }

    #[test]
    fn full_save_append_and_stale_replay_compare_hydrated_content() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("replay.sqlite");
        let header = SessionHeader {
            id: "media-replay".to_string(),
            ..SessionHeader::default()
        };
        let first = fixture("first", 1);
        let second = fixture("second", 1);
        let third = fixture("third", 2);
        futures::executor::block_on(async {
            save_session(&path, &header, std::slice::from_ref(&first), true).await?;
            append_entries(&path, &header.id, std::slice::from_ref(&second), 1).await?;
            // A stale full save must preserve the concurrent second entry.
            save_session(&path, &header, &[first.clone(), third.clone()], false).await?;
            append_entries(&path, &header.id, std::slice::from_ref(&first), 1).await?;
            let (_, loaded) = load_session(&path).await?;
            assert_eq!(
                serde_json::to_value(&loaded)?,
                serde_json::to_value([first, second, third])?
            );
            let error = append_entries(&path, &header.id, &[fixture("first", 9)], 3)
                .await
                .expect_err("changed content under the same entry ID");
            assert!(error.to_string().contains("conflicting persisted content"));
            Ok::<_, Error>(())
        })
        .expect("public SQLite persistence paths");
        run_on_sqlite_thread(|| {
            let conn = map_sqlite_result(SqliteConnection::open_read_only(&path))?;
            assert_eq!(count(&conn), 2);
            Ok(())
        })
        .expect("deduplicated persisted bytes");
    }

    #[test]
    fn legacy_inline_sessions_migrate_on_save_and_copy_without_external_dependencies() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("legacy.sqlite");
        let copy = dir.path().join("copy.sqlite");
        let header = SessionHeader {
            id: "legacy-media".to_string(),
            ..SessionHeader::default()
        };
        let original = fixture("legacy-entry", 7);
        run_on_sqlite_thread(|| {
            let conn = map_sqlite_result(SqliteConnection::open_read_write(&path))?;
            map_sqlite_result(conn.execute_raw(INIT_SQL))?;
            map_sqlite_result(conn.execute_sync(
                "INSERT INTO pi_session_header (id,json) VALUES (?1,?2)",
                &[
                    SqliteValue::from(header.id.clone()),
                    SqliteValue::from(serde_json::to_string(&header)?),
                ],
            ))?;
            map_sqlite_result(conn.execute_sync(
                "INSERT INTO pi_session_entries (seq,json) VALUES (1,?1)",
                &[SqliteValue::from(encoded(&original))],
            ))?;
            map_sqlite_result(conn.close())?;
            Ok(())
        })
        .expect("old inline layout without a blob table");
        futures::executor::block_on(async {
            let (loaded_header, entries) = load_session(&path).await?;
            assert_eq!(encoded(&entries[0]), encoded(&original));
            save_session(&path, &loaded_header, &entries, false).await?;
            save_session(&copy, &loaded_header, &entries, true).await?;
            let (_, copied) = load_session(&copy).await?;
            assert_eq!(encoded(&copied[0]), encoded(&original));
            // JSONL/SDK serialization remains self-contained and reference-free.
            let portable = encoded(&copied[0]);
            assert!(!portable.contains("$piBlob"));
            let reread: SessionEntry = serde_json::from_str(&portable)?;
            assert_eq!(encoded(&reread), encoded(&original));
            Ok::<_, Error>(())
        })
        .expect("migration and portable copy");
        for database in [path, copy] {
            run_on_sqlite_thread(|| {
                let conn = map_sqlite_result(SqliteConnection::open_read_only(&database))?;
                assert_eq!(count(&conn), 1);
                Ok(())
            })
            .expect("each database owns its attachment");
        }
    }
}
