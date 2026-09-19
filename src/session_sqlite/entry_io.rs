//! Bounded serialization and insertion for media-heavy session histories.
//!
//! Reconciliation retains native entries plus an ID-to-index map, not several
//! complete JSON copies. Exact duplicate-ID comparison serializes only one
//! entry at a time; it never replaces byte equality with digest equality.

use serde::Serialize;
use std::fmt::Write as _;
use std::io::{self, Write};

use super::{
    Error, MAX_SQLITE_JSON_BYTES, Result, SessionEntry, SqliteConnection, SqliteValue,
    map_sqlite_result, validate_sqlite_json_for_write,
};

const INSERT_BATCH_ROWS: usize = 200;
const INSERT_BATCH_BYTES: usize = 4 * 1024 * 1024;

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
