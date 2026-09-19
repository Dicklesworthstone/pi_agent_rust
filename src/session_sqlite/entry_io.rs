//! Bounded reads, serialization and insertion for media-heavy session histories.
//!
//! Reconciliation retains native entries plus an ID-to-index map, not several
//! complete JSON copies. Exact duplicate-ID comparison serializes only one
//! entry at a time; it never replaces byte equality with digest equality.

use serde::Serialize;
use std::fmt::Write as _;
use std::io::{self, Write};

use super::{
    Error, MAX_SQLITE_JSON_BYTES, Result, SessionEntry, SqliteConnection, SqliteRow, SqliteValue,
    attachments, map_sqlite_result, rollback_quietly, validate_sqlite_json_for_write,
};

const INSERT_BATCH_ROWS: usize = 200;
const INSERT_BATCH_BYTES: usize = 4 * 1024 * 1024;

/// Keep the header, metadata, row pages and attachment lookups on one read
/// snapshot. Dropping an error path releases the read transaction as well.
pub(super) struct ReadSnapshot<'a> {
    conn: &'a SqliteConnection,
    active: bool,
}

impl<'a> ReadSnapshot<'a> {
    pub(super) fn begin(conn: &'a SqliteConnection) -> Result<Self> {
        map_sqlite_result(conn.execute_raw("BEGIN DEFERRED"))?;
        Ok(Self {
            conn,
            active: true,
        })
    }

    pub(super) fn finish(mut self) -> Result<()> {
        map_sqlite_result(self.conn.execute_raw("COMMIT"))?;
        self.active = false;
        Ok(())
    }
}

impl Drop for ReadSnapshot<'_> {
    fn drop(&mut self) {
        if self.active {
            rollback_quietly(self.conn);
        }
    }
}

#[derive(Clone, Copy)]
struct ReadLimits {
    rows: usize,
    page_bytes: usize,
    entry_bytes: usize,
}

impl Default for ReadLimits {
    fn default() -> Self {
        Self {
            rows: 128,
            page_bytes: 4 * 1024 * 1024,
            entry_bytes: MAX_SQLITE_JSON_BYTES,
        }
    }
}

fn integer_column(row: &SqliteRow, index: usize) -> Result<i64> {
    match row.get(index) {
        Some(SqliteValue::Integer(value)) => Ok(*value),
        _ => Err(Error::session(
            "SQLite entry metadata must contain INTEGER values",
        )),
    }
}

fn check_text_column(row: &SqliteRow, type_index: usize) -> Result<()> {
    if matches!(row.get(type_index), Some(SqliteValue::Text(kind)) if kind.as_str() == "text") {
        Ok(())
    } else {
        Err(Error::session("SQLite session column must use TEXT storage"))
    }
}

/// Inspect sizes before asking the driver to materialize any JSON payload.
/// Validate the entire lookahead, but admit only its byte-bounded prefix.
fn admit_page(rows: &[SqliteRow], first: i64, limits: ReadLimits) -> Result<usize> {
    let mut admitted = 0usize;
    let mut bytes = 0usize;
    let mut full = false;
    for (index, row) in rows.iter().enumerate() {
        let offset = i64::try_from(index)
            .map_err(|_| Error::session("SQLite sequence offset exceeds i64"))?;
        let expected = first
            .checked_add(offset)
            .ok_or_else(|| Error::session("SQLite session sequence overflow"))?;
        let seq = integer_column(row, 0)?;
        if seq != expected {
            return Err(Error::session(format!(
                "SQLite session entry sequence is not contiguous: expected={expected} actual={seq}"
            )));
        }
        check_text_column(row, 2)?;
        let size = usize::try_from(integer_column(row, 1)?)
            .map_err(|_| Error::session("SQLite entry byte length is invalid"))?;
        if size > limits.entry_bytes {
            return Err(Error::session(format!(
                "SQLite session entry exceeds JSON limit: bytes={size} limit={}",
                limits.entry_bytes
            )));
        }
        if !full {
            if admitted == 0 || size <= limits.page_bytes.saturating_sub(bytes) {
                bytes = bytes
                    .checked_add(size)
                    .ok_or_else(|| Error::session("SQLite read page byte count overflow"))?;
                admitted += 1;
            } else {
                full = true;
            }
        }
    }
    Ok(admitted)
}

/// The public load API still returns a full native history. This limits the
/// transient SQL-row buffer, not that result or the database engine's cache.
pub(super) fn read_entries(conn: &SqliteConnection) -> Result<Vec<SessionEntry>> {
    read_entries_with_limits(conn, ReadLimits::default())
}

fn read_entries_with_limits(
    conn: &SqliteConnection,
    limits: ReadLimits,
) -> Result<Vec<SessionEntry>> {
    let row_limit = i64::try_from(limits.rows)
        .ok()
        .filter(|limit| *limit > 0)
        .ok_or_else(|| Error::session("SQLite read page limit must be positive"))?;
    let mut after = None;
    let mut entries = Vec::new();
    loop {
        let metadata = match after {
            None => map_sqlite_result(conn.query_sync(
                "SELECT seq,length(CAST(json AS BLOB)),typeof(json) \
                 FROM pi_session_entries ORDER BY seq ASC LIMIT ?1",
                &[SqliteValue::from(row_limit)],
            ))?,
            Some(seq) => map_sqlite_result(conn.query_sync(
                "SELECT seq,length(CAST(json AS BLOB)),typeof(json) \
                 FROM pi_session_entries WHERE seq > ?1 ORDER BY seq ASC LIMIT ?2",
                &[SqliteValue::from(seq), SqliteValue::from(row_limit)],
            ))?,
        };
        if metadata.is_empty() {
            return Ok(entries);
        }
        let first = after
            .map_or(Some(1), |seq: i64| seq.checked_add(1))
            .ok_or_else(|| Error::session("SQLite session sequence overflow"))?;
        let admitted = admit_page(&metadata, first, limits)?;
        let last = integer_column(&metadata[admitted - 1], 0)?;
        let count = i64::try_from(admitted)
            .map_err(|_| Error::session("SQLite page row count exceeds i64"))?;
        let rows = map_sqlite_result(conn.query_sync(
            "SELECT seq,json FROM pi_session_entries \
             WHERE seq >= ?1 AND seq <= ?2 ORDER BY seq ASC LIMIT ?3",
            &[
                SqliteValue::from(first),
                SqliteValue::from(last),
                SqliteValue::from(count),
            ],
        ))?;
        if rows.len() != admitted {
            return Err(Error::session("SQLite entry page changed during read"));
        }
        for (index, row) in rows.into_iter().enumerate() {
            if integer_column(&row, 0)? != integer_column(&metadata[index], 0)? {
                return Err(Error::session("SQLite entry sequence changed during read"));
            }
            let Some(SqliteValue::Text(json)) = row.get(1) else {
                return Err(Error::session("SQLite session entry must use TEXT storage"));
            };
            let expected = usize::try_from(integer_column(&metadata[index], 1)?)
                .map_err(|_| Error::session("SQLite entry byte length is invalid"))?;
            if json.len() != expected {
                return Err(Error::session("SQLite entry length changed during read"));
            }
            // Borrow the row's string: hydration must not first clone a large
            // inline payload just to pass it to the native decoder.
            entries.push(attachments::decode_entry(conn, json.as_str())?);
        }
        after = Some(last);
    }
}

/// A header table should contain zero or one row. Neither a corrupt duplicate
/// table nor an oversized TEXT/BLOB may force an unbounded payload query.
pub(super) fn preflight_header(conn: &SqliteConnection) -> Result<()> {
    let rows = map_sqlite_result(conn.query_sync(
        "SELECT length(CAST(id AS BLOB)),length(CAST(json AS BLOB)),typeof(id),typeof(json) \
         FROM pi_session_header LIMIT 2",
        &[],
    ))?;
    if rows.len() > 1 {
        return Err(Error::session(
            "SQLite session contains multiple header rows",
        ));
    }
    if let Some(row) = rows.first() {
        for index in 0..2 {
            check_text_column(row, index + 2)?;
            let bytes = usize::try_from(integer_column(row, index)?)
                .map_err(|_| Error::session("SQLite header byte length is invalid"))?;
            super::validate_sqlite_json_length("session header", bytes)?;
        }
    }
    Ok(())
}

struct BoundedWriter<W> {
    inner: W,
    written: usize,
    limit: usize,
    exceeded: bool,
}

impl<W: Write> Write for BoundedWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let Some(total) = self
            .written
            .checked_add(bytes.len())
            .filter(|n| *n <= self.limit)
        else {
            self.exceeded = true;
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "JSON size limit exceeded",
            ));
        };
        self.inner.write_all(bytes)?;
        self.written = total;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn serialize_bounded<T: Serialize + ?Sized, W: Write>(
    value: &T,
    inner: W,
    limit: usize,
) -> Result<(W, usize)> {
    let mut writer = BoundedWriter {
        inner,
        written: 0,
        limit,
        exceeded: false,
    };
    if let Err(error) = serde_json::to_writer(&mut writer, value) {
        if writer.exceeded {
            return Err(Error::session(format!(
                "SQLite session entry exceeds JSON limit: limit={limit}"
            )));
        }
        return Err(error.into());
    }
    Ok((writer.inner, writer.written))
}

/// Count exactly what serde writes, including escaping, without retaining it.
pub(super) fn serialized_len(entry: &SessionEntry) -> Result<usize> {
    serialize_bounded(entry, io::sink(), MAX_SQLITE_JSON_BYTES).map(|(_, size)| size)
}

pub(super) fn encode_entry(entry: &SessionEntry) -> Result<String> {
    let (bytes, _) = serialize_bounded(entry, Vec::new(), MAX_SQLITE_JSON_BYTES)?;
    String::from_utf8(bytes)
        .map_err(|_| Error::session("SQLite JSON serializer produced invalid UTF-8"))
}

struct ExactMatch<'a> {
    expected: &'a [u8],
    offset: usize,
    matched: bool,
}

impl Write for ExactMatch<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let end = self.offset.checked_add(bytes.len()).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "JSON comparison size overflow")
        })?;
        self.matched &= self.expected.get(self.offset..end) == Some(bytes);
        self.offset = end;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) fn canonical_matches(stored: &SessionEntry, incoming: &SessionEntry) -> Result<bool> {
    let expected = encode_entry(stored)?;
    let comparison = ExactMatch {
        expected: expected.as_bytes(),
        offset: 0,
        matched: true,
    };
    let (comparison, size) = serialize_bounded(incoming, comparison, MAX_SQLITE_JSON_BYTES)?;
    Ok(comparison.matched && size == expected.len())
}

/// A batch is bounded by bytes as well as row count. One individually admitted
/// large row is written alone; it cannot drag another 199 large rows with it.
/// The caller's transaction owns every flush and every attachment insertion.
pub(super) struct InsertBatch<'a> {
    conn: &'a SqliteConnection,
    params: Vec<SqliteValue>,
    bytes: usize,
}

impl<'a> InsertBatch<'a> {
    pub(super) fn new(conn: &'a SqliteConnection) -> Self {
        Self {
            conn,
            params: Vec::new(),
            bytes: 0,
        }
    }

    pub(super) fn push(&mut self, seq: i64, json: String) -> Result<()> {
        validate_sqlite_json_for_write("stored session entry", &json)?;
        if !self.params.is_empty()
            && (self.params.len() / 2 >= INSERT_BATCH_ROWS
                || json.len() > INSERT_BATCH_BYTES.saturating_sub(self.bytes))
        {
            self.flush()?;
        }
        self.bytes = self
            .bytes
            .checked_add(json.len())
            .ok_or_else(|| Error::session("SQLite insert batch byte count overflow"))?;
        self.params.push(SqliteValue::from(seq));
        self.params.push(SqliteValue::from(json));
        if self.params.len() / 2 == INSERT_BATCH_ROWS || self.bytes >= INSERT_BATCH_BYTES {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        if self.params.is_empty() {
            return Ok(());
        }
        let rows = self.params.len() / 2;
        let mut sql = String::with_capacity(64 + rows * 16);
        sql.push_str("INSERT INTO pi_session_entries (seq,json) VALUES ");
        for index in 0..rows {
            if index > 0 {
                sql.push(',');
            }
            let _ = write!(sql, "(?{},?{})", index * 2 + 1, index * 2 + 2);
        }
        map_sqlite_result(self.conn.execute_sync(&sql, &self.params))?;
        self.params.clear();
        self.bytes = 0;
        Ok(())
    }

    pub(super) fn finish(mut self) -> Result<()> {
        self.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::UserContent;
    use crate::session::{EntryBase, MessageEntry, SessionMessage};
    use crate::session_sqlite::{INIT_SQL, run_on_sqlite_thread};

    fn entry(id: &str, text: &str) -> SessionEntry {
        SessionEntry::Message(MessageEntry {
            base: EntryBase {
                id: Some(id.to_string()),
                parent_id: None,
                timestamp: "2026-09-19T00:00:00.000Z".to_string(),
            },
            message: SessionMessage::User {
                content: UserContent::Text(text.to_string()),
                timestamp: None,
            },
        })
    }

    fn with_database(
        f: impl FnOnce(&SqliteConnection, &std::path::Path) -> Result<()> + Send,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("paged.sqlite");
        run_on_sqlite_thread(|| {
            let conn = map_sqlite_result(SqliteConnection::open_read_write(&path))?;
            map_sqlite_result(conn.execute_raw(INIT_SQL))?;
            f(&conn, &path)?;
            map_sqlite_result(conn.close())
        })
        .expect("SQLite paging fixture");
    }

    fn insert_fixture(conn: &SqliteConnection, seq: i64, value: SqliteValue) -> Result<()> {
        map_sqlite_result(conn.execute_sync(
            "INSERT INTO pi_session_entries (seq,json) VALUES (?1,?2)",
            &[SqliteValue::from(seq), value],
        ))?;
        Ok(())
    }

    fn metadata(conn: &SqliteConnection) -> Result<Vec<SqliteRow>> {
        map_sqlite_result(conn.query_sync(
            "SELECT seq,length(CAST(json AS BLOB)),typeof(json) \
             FROM pi_session_entries ORDER BY seq ASC LIMIT 128",
            &[],
        ))
    }

    const fn small_limits() -> ReadLimits {
        ReadLimits {
            rows: 2,
            page_bytes: 512,
            entry_bytes: 4096,
        }
    }

    #[test]
    fn paged_history_preserves_every_entry_in_order() {
        with_database(|conn, _| {
            let expected: Vec<_> = (1..=9)
                .map(|index| encode_entry(&entry(&index.to_string(), "音声🙂")))
                .collect::<Result<_>>()?;
            for (index, json) in expected.iter().enumerate() {
                insert_fixture(
                    conn,
                    i64::try_from(index + 1).expect("sequence"),
                    SqliteValue::from(json.clone()),
                )?;
            }
            // A one-byte page budget forces each individually admitted row to
            // stand alone, independently of the two-row metadata lookahead.
            let limits = ReadLimits {
                page_bytes: 1,
                ..small_limits()
            };
            let snapshot = ReadSnapshot::begin(conn)?;
            let restored = read_entries_with_limits(conn, limits)?;
            snapshot.finish()?;
            let actual = restored
                .iter()
                .map(encode_entry)
                .collect::<Result<Vec<_>>>()?;
            assert_eq!(actual, expected);
            Ok(())
        });
    }

    #[test]
    fn page_admission_stops_at_the_first_row_that_exceeds_its_budget() {
        with_database(|conn, _| {
            for (seq, size) in [(1, 4), (2, 5), (3, 20), (4, 1)] {
                insert_fixture(conn, seq, SqliteValue::from("x".repeat(size)))?;
            }
            let rows = metadata(conn)?;
            let limits = ReadLimits {
                page_bytes: 10,
                ..small_limits()
            };
            assert_eq!(admit_page(&rows, 1, limits)?, 2);
            // Do not skip the big row and take a later small row instead.
            assert_eq!(admit_page(&rows[2..], 3, limits)?, 1);
            assert_eq!(admit_page(&rows[3..], 4, limits)?, 1);
            assert_eq!(
                admit_page(
                    &rows[..2],
                    1,
                    ReadLimits {
                        page_bytes: 9,
                        ..limits
                    },
                )?,
                2,
                "the exact byte boundary is admitted"
            );
            Ok(())
        });
    }

    #[test]
    fn sequence_gaps_and_nonpositive_rows_are_not_hidden_by_pagination() {
        for sequences in [vec![0, 1], vec![-1, 1], vec![2], vec![1, 2, 4]] {
            with_database(|conn, _| {
                for seq in sequences {
                    insert_fixture(
                        conn,
                        seq,
                        SqliteValue::from(encode_entry(&entry(&seq.to_string(), "text"))?),
                    )?;
                }
                let error = read_entries_with_limits(conn, small_limits())
                    .expect_err("noncontiguous persisted history");
                assert!(error.to_string().contains("not contiguous"), "{error}");
                Ok(())
            });
        }
    }

    #[test]
    fn preflight_uses_utf8_bytes_not_sql_text_characters() {
        with_database(|conn, _| {
            let json = encode_entry(&entry("unicode", &"🙂".repeat(40)))?;
            let character_count = json.chars().count();
            assert!(json.len() > character_count);
            insert_fixture(conn, 1, SqliteValue::from(json.clone()))?;
            let limits = ReadLimits {
                entry_bytes: json.len() - 1,
                ..small_limits()
            };
            assert!(limits.entry_bytes > character_count);
            let error = read_entries_with_limits(conn, limits).expect_err("byte cap");
            assert!(error.to_string().contains("exceeds JSON limit"));
            assert!(!error.to_string().contains('🙂'));
            let restored = read_entries_with_limits(
                conn,
                ReadLimits {
                    entry_bytes: json.len(),
                    ..limits
                },
            )?;
            assert_eq!(encode_entry(&restored[0])?, json);
            Ok(())
        });
    }

    #[test]
    fn binary_entry_columns_fail_without_echoing_their_payload() {
        with_database(|conn, _| {
            let secret = b"private-attachment-content".to_vec();
            insert_fixture(conn, 1, SqliteValue::Blob(secret.into()))?;
            let error = read_entries_with_limits(conn, small_limits())
                .expect_err("TEXT storage required");
            assert!(error.to_string().contains("TEXT storage"));
            assert!(!error.to_string().contains("private-attachment"));
            Ok(())
        });
    }

    #[test]
    fn malformed_later_page_fails_instead_of_returning_a_partial_history() {
        with_database(|conn, _| {
            for seq in 1..=2 {
                insert_fixture(
                    conn,
                    seq,
                    SqliteValue::from(encode_entry(&entry(&seq.to_string(), "valid"))?),
                )?;
            }
            insert_fixture(conn, 3, SqliteValue::from("{private-payload"))?;
            let snapshot = ReadSnapshot::begin(conn)?;
            let error = read_entries_with_limits(conn, small_limits())
                .expect_err("later page must not become a successful prefix");
            assert!(!error.to_string().contains("private-payload"));
            drop(snapshot);
            // The caller can start another transaction after the failed read.
            ReadSnapshot::begin(conn)?.finish()?;
            Ok(())
        });
    }

    #[test]
    fn empty_history_and_invalid_page_limits_are_distinct() {
        with_database(|conn, _| {
            assert!(read_entries_with_limits(conn, small_limits())?.is_empty());
            let error = read_entries_with_limits(
                conn,
                ReadLimits {
                    rows: 0,
                    ..small_limits()
                },
            )
            .expect_err("invalid configuration is not an empty history");
            assert!(error.to_string().contains("must be positive"));
            Ok(())
        });
    }

    #[test]
    fn dropping_a_read_snapshot_releases_its_transaction() {
        with_database(|conn, _| {
            let snapshot = ReadSnapshot::begin(conn)?;
            assert!(read_entries(conn)?.is_empty());
            drop(snapshot);
            map_sqlite_result(conn.execute_raw("BEGIN IMMEDIATE"))?;
            insert_fixture(
                conn,
                1,
                SqliteValue::from(encode_entry(&entry("one", "saved"))?),
            )?;
            map_sqlite_result(conn.execute_raw("COMMIT"))?;
            let snapshot = ReadSnapshot::begin(conn)?;
            assert_eq!(read_entries(conn)?.len(), 1);
            snapshot.finish()
        });
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn read_only_snapshot_keeps_header_entries_and_blobs_on_one_generation() {
        use crate::model::{ContentBlock, ImageContent};
        use crate::session::SessionHeader;
        use base64::Engine as _;

        with_database(|writer, path| {
            let header = SessionHeader {
                id: "snapshot-original".to_string(),
                ..SessionHeader::default()
            };
            let mut original = entry("media", "");
            let SessionEntry::Message(message) = &mut original else {
                panic!("message fixture");
            };
            let SessionMessage::User { content, .. } = &mut message.message else {
                panic!("user fixture");
            };
            *content = UserContent::Blocks(vec![ContentBlock::Image(ImageContent {
                data: base64::engine::general_purpose::STANDARD.encode(vec![9; 70 * 1024]),
                mime_type: "image/png".to_string(),
            })]);
            map_sqlite_result(writer.execute_raw("BEGIN IMMEDIATE"))?;
            map_sqlite_result(writer.execute_sync(
                "INSERT INTO pi_session_header (id,json) VALUES (?1,?2)",
                &[
                    SqliteValue::from(header.id.clone()),
                    SqliteValue::from(serde_json::to_string(&header)?),
                ],
            ))?;
            let stored =
                attachments::EntryEncoder::new(writer).encode(&encode_entry(&original)?)?;
            assert!(
                stored.contains("$piBlob"),
                "fixture must exercise blob hydration"
            );
            insert_fixture(writer, 1, SqliteValue::from(stored))?;
            map_sqlite_result(writer.execute_raw("COMMIT"))?;

            let reader = map_sqlite_result(SqliteConnection::open_read_only(path))?;
            let snapshot = ReadSnapshot::begin(&reader)?;
            let pinned = crate::session_sqlite::read_stored_header(&reader)?
                .expect("header establishes the read snapshot");
            assert_eq!(pinned.id, header.id);

            // A different connection commits after the header is read but
            // before entry pages and attachment payloads are loaded.
            let replacement = SessionHeader {
                id: "snapshot-replacement".to_string(),
                ..SessionHeader::default()
            };
            map_sqlite_result(writer.execute_raw("BEGIN IMMEDIATE"))?;
            map_sqlite_result(writer.execute_sync(
                "UPDATE pi_session_header SET id=?1,json=?2",
                &[
                    SqliteValue::from(replacement.id.clone()),
                    SqliteValue::from(serde_json::to_string(&replacement)?),
                ],
            ))?;
            insert_fixture(
                writer,
                2,
                SqliteValue::from(encode_entry(&entry("new", "later"))?),
            )?;
            map_sqlite_result(writer.execute_sync(
                "UPDATE pi_session_blobs SET data=?1",
                &[SqliteValue::Blob(vec![8; 70 * 1024].into())],
            ))?;
            map_sqlite_result(writer.execute_raw("COMMIT"))?;

            let entries = read_entries_with_limits(
                &reader,
                ReadLimits {
                    rows: 1,
                    ..ReadLimits::default()
                },
            )?;
            assert_eq!(
                entries.len(),
                1,
                "the later append is not in this snapshot"
            );
            assert_eq!(encode_entry(&entries[0])?, encode_entry(&original)?);
            snapshot.finish()?;
            drop(reader);

            let fresh = map_sqlite_result(SqliteConnection::open_read_only(path))?;
            let snapshot = ReadSnapshot::begin(&fresh)?;
            assert_eq!(
                crate::session_sqlite::read_stored_header(&fresh)?
                    .expect("new header")
                    .id,
                replacement.id
            );
            let error = read_entries(&fresh).expect_err("new snapshot sees the corrupted blob");
            assert!(error.to_string().contains("ATTACHMENT_INVALID"));
            snapshot.finish()?;
            Ok(())
        });
    }

    #[test]
    fn header_preflight_rejects_nontext_payloads_before_deserialization() {
        with_database(|conn, _| {
            map_sqlite_result(conn.execute_sync(
                "INSERT INTO pi_session_header (id,json) VALUES (?1,?2)",
                &[
                    SqliteValue::from("header"),
                    SqliteValue::Blob(b"private-header-content".to_vec().into()),
                ],
            ))?;
            let error = crate::session_sqlite::read_stored_header(conn)
                .expect_err("nontext header");
            assert!(error.to_string().contains("TEXT storage"));
            assert!(!error.to_string().contains("private-header"));
            Ok(())
        });
    }

    #[test]
    fn duplicate_headers_are_rejected_without_parsing_either_payload() {
        with_database(|conn, _| {
            for id in ["first", "second"] {
                map_sqlite_result(conn.execute_sync(
                    "INSERT INTO pi_session_header (id,json) VALUES (?1,?2)",
                    &[SqliteValue::from(id), SqliteValue::from("not JSON")],
                ))?;
            }
            let error = crate::session_sqlite::read_stored_header(conn)
                .expect_err("duplicate header table");
            assert!(error.to_string().contains("multiple header rows"));
            Ok(())
        });
    }

    #[test]
    fn counting_and_encoding_agree_on_escaped_unicode_content() {
        let entry = entry("unicode", "音声\n\t\\\"\0🙂");
        let expected = serde_json::to_string(&entry).expect("encode fixture");
        assert_eq!(serialized_len(&entry).expect("count"), expected.len());
        assert_eq!(encode_entry(&entry).expect("bounded encode"), expected);
        assert!(canonical_matches(&entry, &entry).expect("exact identity"));
    }

    #[test]
    fn limit_is_checked_before_the_sink_receives_excess_bytes() {
        let mut bytes = Vec::new();
        let mut writer = BoundedWriter {
            inner: &mut bytes,
            written: 0,
            limit: 3,
            exceeded: false,
        };
        writer.write_all(b"abc").expect("exact limit");
        assert!(writer.write_all(b"secret").is_err());
        assert!(writer.exceeded);
        assert_eq!(writer.written, 3);
        assert_eq!(bytes, b"abc");
    }

    #[test]
    fn serde_count_limit_covers_escaping_and_exact_boundary() {
        let value = "\n\0\"🙂";
        let expected = serde_json::to_vec(value).expect("JSON");
        let (_, size) = serialize_bounded(value, io::sink(), expected.len()).expect("exact cap");
        assert_eq!(size, expected.len());
        let error = serialize_bounded(value, io::sink(), expected.len() - 1)
            .expect_err("escaped representation exceeds limit");
        assert!(error.to_string().contains("exceeds JSON limit"));
        assert!(!error.to_string().contains('🙂'));
    }

    #[test]
    fn duplicate_ids_still_require_exact_content_not_just_length() {
        let first = entry("same", "one");
        let second = entry("same", "two");
        assert_eq!(
            serialized_len(&first).expect("first size"),
            serialized_len(&second).expect("second size")
        );
        assert!(!canonical_matches(&first, &second).expect("compare"));
        for other in [
            entry("other", "one"),
            entry("same", "one more"),
            entry("same", "on"),
        ] {
            assert!(!canonical_matches(&first, &other).expect("compare"));
            assert!(!canonical_matches(&other, &first).expect("reverse compare"));
        }
    }

    #[test]
    fn exact_match_is_independent_of_serializer_write_boundaries() {
        for chunks in [
            vec!["a", "bc"],
            vec!["ab", "c"],
            vec!["abc"],
            vec!["", "abc", ""],
        ] {
            let mut matched = ExactMatch {
                expected: b"abc",
                offset: 0,
                matched: true,
            };
            for chunk in chunks {
                matched.write_all(chunk.as_bytes()).expect("write");
            }
            assert!(matched.matched);
            assert_eq!(matched.offset, 3);
        }
        let mut matched = ExactMatch {
            expected: b"abc",
            offset: 0,
            matched: true,
        };
        matched.write_all(b"abd").expect("mismatch");
        matched.write_all(b"extra").expect("longer suffix");
        assert!(!matched.matched);
    }

    #[test]
    fn insert_batches_flush_by_rows_and_preserve_sequences() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("batch.sqlite");
        run_on_sqlite_thread(|| {
            let conn = map_sqlite_result(SqliteConnection::open_read_write(&path))?;
            map_sqlite_result(conn.execute_raw(INIT_SQL))?;
            map_sqlite_result(conn.execute_raw("BEGIN IMMEDIATE"))?;
            let mut batch = InsertBatch::new(&conn);
            for index in 1..=INSERT_BATCH_ROWS + 3 {
                batch.push(
                    i64::try_from(index).expect("sequence"),
                    format!("{{\"row\":{index}}}"),
                )?;
            }
            assert_eq!(batch.params.len(), 6, "first 200 rows were flushed");
            batch.finish()?;
            let rows = map_sqlite_result(conn.query_sync(
                "SELECT seq FROM pi_session_entries ORDER BY seq",
                &[],
            ))?;
            assert_eq!(rows.len(), INSERT_BATCH_ROWS + 3);
            for (index, row) in rows.iter().enumerate() {
                let expected = i64::try_from(index + 1).expect("sequence");
                assert!(matches!(row.get(0), Some(SqliteValue::Integer(n)) if *n == expected));
            }
            map_sqlite_result(conn.execute_raw("ROLLBACK"))?;
            map_sqlite_result(conn.close())
        })
        .expect("batch fixture");
    }

    #[test]
    fn oversized_individual_rows_are_not_aggregated_into_large_batches() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("large.sqlite");
        run_on_sqlite_thread(|| {
            let conn = map_sqlite_result(SqliteConnection::open_read_write(&path))?;
            map_sqlite_result(conn.execute_raw(INIT_SQL))?;
            map_sqlite_result(conn.execute_raw("BEGIN IMMEDIATE"))?;
            let mut batch = InsertBatch::new(&conn);
            batch.push(1, "{}".to_string())?;
            batch.push(2, " ".repeat(INSERT_BATCH_BYTES + 1))?;
            assert!(batch.params.is_empty(), "large admitted row flushed alone");
            assert_eq!(batch.bytes, 0);
            batch.push(3, "{}".to_string())?;
            assert_eq!(batch.params.len(), 2);
            batch.finish()?;
            // A later failure/rollback must remove even previously flushed batches.
            map_sqlite_result(conn.execute_raw("ROLLBACK"))?;
            assert!(
                map_sqlite_result(conn.query_sync("SELECT seq FROM pi_session_entries", &[]))?
                    .is_empty()
            );
            map_sqlite_result(conn.close())
        })
        .expect("large row fixture");
    }
}
