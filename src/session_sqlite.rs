use crate::error::{Error, Result};
use crate::session::{SessionEntry, SessionHeader};
use crate::session_metrics;
use sha2::{Digest as _, Sha256};
use sqlmodel_core::{Error as SqliteError, Row as SqliteRow, Value as SqliteValue};
use sqlmodel_sqlite::{OpenFlags, SqliteConfig, SqliteConnection};
use std::fmt::Write as _;
use std::io::Read as _;
use std::path::{Path, PathBuf};

const INIT_SQL: &str = r"
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS pi_session_header (
  id TEXT PRIMARY KEY,
  json TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS pi_session_entries (
  seq INTEGER PRIMARY KEY,
  json TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS pi_session_meta (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
";

const MAX_SQLITE_JSON_BYTES: usize = 100 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct SqliteSessionMeta {
    pub header: SessionHeader,
    pub message_count: u64,
    pub name: Option<String>,
}

fn map_sqlite_result<T>(result: std::result::Result<T, SqliteError>) -> Result<T> {
    result.map_err(|err| Error::session(format!("SQLite session error: {err}")))
}

fn ensure_regular_sqlite_artifact_if_present(path: &Path) -> Result<bool> {
    if !crate::session::session_path_entry_exists(path).map_err(|err| Error::Io(Box::new(err)))? {
        return Ok(false);
    }
    let metadata = std::fs::symlink_metadata(path).map_err(|err| Error::Io(Box::new(err)))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(Error::Io(Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "SQLite session artifacts must be regular files, not symlinks or special files: {}",
                path.display()
            ),
        ))));
    }
    Ok(true)
}

fn open_sqlite_connection_read_only(path: &Path) -> Result<SqliteConnection> {
    ensure_regular_sqlite_artifact_if_present(path)?;
    crate::session::ensure_session_file_readable(path).map_err(|err| Error::Io(Box::new(err)))?;
    let wal_and_shm_exist = ensure_preexisting_sqlite_sidecar_access(path, false)?;
    if sqlite_database_uses_wal(path)? && !wal_and_shm_exist {
        // This connection is not opened with SQLite's immutable URI option.
        // A WAL-mode read may therefore need to create whichever sidecar is
        // absent. Existing WAL and SHM files only need to be readable.
        crate::session::ensure_session_parent_writable(path)
            .map_err(|err| Error::Io(Box::new(err)))?;
    }
    let config = SqliteConfig::file(path.to_string_lossy()).flags(OpenFlags::read_only());
    map_sqlite_result(SqliteConnection::open(&config))
}

fn sqlite_database_uses_wal(path: &Path) -> Result<bool> {
    let mut file = crate::session::open_existing_session_file_for_read(path)?;
    let mut header = [0u8; 20];
    file.read_exact(&mut header).map_err(|err| {
        Error::session(format!(
            "Failed to read SQLite database header {}: {err}",
            path.display()
        ))
    })?;
    if &header[..16] != b"SQLite format 3\0" {
        return Err(Error::Io(Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("Invalid SQLite database header: {}", path.display()),
        ))));
    }
    match (header[18], header[19]) {
        (1, 1) => Ok(false),
        (2, 2) => Ok(true),
        (write_version, read_version) => Err(Error::Io(Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "Unsupported SQLite file format versions for {}: write={write_version} read={read_version}",
                path.display()
            ),
        )))),
    }
}

fn open_sqlite_connection_read_write(path: &Path) -> Result<SqliteConnection> {
    // SQLite may create or update WAL/SHM sidecars beside an existing database,
    // so the parent must remain creatable as well as the database read-write.
    crate::session::ensure_session_parent_writable(path).map_err(|err| Error::Io(Box::new(err)))?;
    if ensure_regular_sqlite_artifact_if_present(path)? {
        crate::session::ensure_session_file_read_write(path)
            .map_err(|err| Error::Io(Box::new(err)))?;
    }
    ensure_preexisting_sqlite_sidecar_access(path, true)?;
    let config = SqliteConfig::file(path.to_string_lossy()).flags(OpenFlags::create_read_write());
    map_sqlite_result(SqliteConnection::open(&config))
}

fn row_get_string(row: &SqliteRow, column: &str) -> Result<String> {
    row.get_named::<String>(column)
        .map_err(|err| Error::session(format!("SQLite row read failed: {err}")))
}

fn malformed_json_error(kind: &str, json: &str, error: &serde_json::Error) -> Error {
    let digest = Sha256::digest(json.as_bytes());
    Error::session(format!(
        "Failed to parse SQLite {kind}: line={} column={} bytes={} sha256={digest:x}",
        error.line(),
        error.column(),
        json.len()
    ))
}

fn validate_sqlite_json_length(kind: &str, byte_length: usize) -> Result<()> {
    if byte_length > MAX_SQLITE_JSON_BYTES {
        return Err(Error::session(format!(
            "SQLite {kind} exceeds JSON limit: bytes={byte_length} limit={MAX_SQLITE_JSON_BYTES}"
        )));
    }
    Ok(())
}

fn validate_sqlite_json_for_write(kind: &str, json: &str) -> Result<()> {
    validate_sqlite_json_length(kind, json.len())
}

fn parse_sqlite_json<T: serde::de::DeserializeOwned>(kind: &str, json: &str) -> Result<T> {
    if json.len() > MAX_SQLITE_JSON_BYTES {
        let digest = Sha256::digest(json.as_bytes());
        return Err(Error::session(format!(
            "SQLite {kind} exceeds JSON limit: bytes={} limit={MAX_SQLITE_JSON_BYTES} sha256={digest:x}",
            json.len()
        )));
    }
    serde_json::from_str(json).map_err(|error| malformed_json_error(kind, json, &error))
}

fn read_stored_header(conn: &SqliteConnection) -> Result<Option<SessionHeader>> {
    let mut rows = map_sqlite_result(
        conn.query_sync("SELECT id,json FROM pi_session_header ORDER BY id", &[]),
    )?;
    if rows.len() > 1 {
        return Err(Error::session(
            "SQLite session contains multiple header rows",
        ));
    }
    let Some(row) = rows.pop() else {
        return Ok(None);
    };
    let row_id = row_get_string(&row, "id")?;
    let header_json = row_get_string(&row, "json")?;
    let header: SessionHeader = parse_sqlite_json("session header", &header_json)?;
    header
        .validate()
        .map_err(|reason| Error::session(format!("Invalid session header: {reason}")))?;
    if row_id != header.id {
        return Err(Error::session(
            "SQLite session header row ID does not match its serialized header ID",
        ));
    }
    Ok(Some(header))
}

fn rollback_quietly(conn: &SqliteConnection) {
    let _ = conn.execute_raw("ROLLBACK");
}

fn sqlite_artifact_paths(path: &Path) -> [PathBuf; 4] {
    [
        path.to_path_buf(),
        append_sidecar_suffix(path, "-wal"),
        append_sidecar_suffix(path, "-shm"),
        append_sidecar_suffix(path, "-journal"),
    ]
}

fn append_sidecar_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut sidecar = path.as_os_str().to_os_string();
    sidecar.push(suffix);
    PathBuf::from(sidecar)
}

fn ensure_preexisting_sqlite_sidecar_access(path: &Path, writable: bool) -> Result<bool> {
    let [wal_path, shm_path] = [
        append_sidecar_suffix(path, "-wal"),
        append_sidecar_suffix(path, "-shm"),
    ];
    let mut existing_sidecars = 0usize;
    for sidecar in [&wal_path, &shm_path] {
        if !ensure_regular_sqlite_artifact_if_present(sidecar)? {
            continue;
        }
        existing_sidecars = existing_sidecars.saturating_add(1);

        // SQLite 3.22+ can open a read-only WAL database when both existing
        // sidecars are readable. A read-write connection may mutate either.
        let access = if writable {
            crate::session::ensure_session_file_read_write(sidecar)
        } else {
            crate::session::ensure_session_file_readable(sidecar)
        };
        access.map_err(|err| Error::Io(Box::new(err)))?;
    }
    let rollback_journal = append_sidecar_suffix(path, "-journal");
    if ensure_regular_sqlite_artifact_if_present(&rollback_journal)? {
        // A rollback journal is part of the database's recovery state. SQLite
        // may read it during a read-only open and may update it during a save.
        let access = if writable {
            crate::session::ensure_session_file_read_write(&rollback_journal)
        } else {
            crate::session::ensure_session_file_readable(&rollback_journal)
        };
        access.map_err(|err| Error::Io(Box::new(err)))?;
    }
    Ok(existing_sidecars == 2)
}

#[cfg(unix)]
fn set_private_permissions_if_present(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    if ensure_regular_sqlite_artifact_if_present(path)? {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|err| {
            Error::session(format!(
                "Failed to secure SQLite session artifact {}: {err}",
                path.display()
            ))
        })?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_private_permissions_if_present(_path: &Path) -> Result<()> {
    Ok(())
}

fn ensure_private_sqlite_permissions(path: &Path) -> Result<()> {
    for artifact in sqlite_artifact_paths(path) {
        set_private_permissions_if_present(&artifact)?;
    }
    Ok(())
}

#[derive(Debug)]
struct StoredEntry {
    entry: SessionEntry,
    canonical_json: String,
}

fn read_stored_entries(conn: &SqliteConnection) -> Result<Vec<StoredEntry>> {
    let entry_rows = map_sqlite_result(conn.query_sync(
        "SELECT seq,json FROM pi_session_entries ORDER BY seq ASC",
        &[],
    ))?;

    let mut entries = Vec::with_capacity(entry_rows.len());
    for (index, row) in entry_rows.into_iter().enumerate() {
        let seq = row
            .get_named::<i64>("seq")
            .map_err(|err| Error::session(format!("SQLite row read failed: {err}")))?;
        let expected_seq = i64::try_from(index)
            .map_err(|_| Error::session("SQLite session entry count exceeds i64"))?
            .checked_add(1)
            .ok_or_else(|| Error::session("SQLite session sequence overflow"))?;
        if seq != expected_seq {
            return Err(Error::session(format!(
                "SQLite session entry sequence is not contiguous: expected={expected_seq} actual={seq}"
            )));
        }
        let json = row_get_string(&row, "json")?;
        let entry: SessionEntry = parse_sqlite_json("session entry", &json)?;
        let canonical_json = serde_json::to_string(&entry)?;
        entries.push(StoredEntry {
            entry,
            canonical_json,
        });
    }
    Ok(entries)
}

fn read_all_entries(conn: &SqliteConnection) -> Result<Vec<SessionEntry>> {
    Ok(read_stored_entries(conn)?
        .into_iter()
        .map(|stored| stored.entry)
        .collect())
}

fn is_missing_meta_table_error(err: &SqliteError) -> bool {
    err.to_string().contains("no such table: pi_session_meta")
}

fn query_session_meta_rows(conn: &SqliteConnection) -> Result<Vec<SqliteRow>> {
    match conn.query_sync(
        "SELECT key,value FROM pi_session_meta WHERE key IN ('message_count','name')",
        &[],
    ) {
        Ok(rows) => Ok(rows),
        Err(err) if is_missing_meta_table_error(&err) => Ok(Vec::new()),
        Err(err) => Err(Error::session(format!(
            "SQLite session meta query failed: {err}"
        ))),
    }
}

fn compute_message_count_and_name(entries: &[SessionEntry]) -> (u64, Option<String>) {
    let mut message_count = 0u64;
    let mut name = None;

    for entry in entries {
        match entry {
            SessionEntry::Message(_) => message_count += 1,
            SessionEntry::SessionInfo(info) if info.name.is_some() => {
                name.clone_from(&info.name);
            }
            _ => {}
        }
    }

    (message_count, name)
}

struct ReconciledEntries {
    entries: Vec<SessionEntry>,
    json: Vec<String>,
    appended_json: Vec<String>,
}

fn validate_unique_incoming_entry_ids(entries: &[SessionEntry], context: &str) -> Result<()> {
    let mut ids = std::collections::HashSet::with_capacity(entries.len());
    for entry in entries {
        let id = entry
            .base_id()
            .ok_or_else(|| Error::session(format!("{context} entry is missing its ID")))?;
        if !ids.insert(id.as_str()) {
            return Err(Error::session(format!(
                "{context} contains duplicate entry ID {id}"
            )));
        }
    }
    Ok(())
}

fn reconcile_entries(
    stored_entries: Vec<StoredEntry>,
    incoming_entries: &[SessionEntry],
) -> Result<ReconciledEntries> {
    let mut entries = Vec::with_capacity(stored_entries.len() + incoming_entries.len());
    let mut json = Vec::with_capacity(stored_entries.len() + incoming_entries.len());
    let mut by_id =
        std::collections::HashMap::with_capacity(stored_entries.len() + incoming_entries.len());

    for stored in stored_entries {
        let id = stored
            .entry
            .base_id()
            .ok_or_else(|| Error::session("persisted SQLite session entry is missing its ID"))?;
        if by_id
            .insert(id.clone(), stored.canonical_json.clone())
            .is_some()
        {
            return Err(Error::session(format!(
                "persisted SQLite session contains duplicate entry ID {id}"
            )));
        }
        entries.push(stored.entry);
        json.push(stored.canonical_json);
    }

    let mut appended_json = Vec::new();
    for incoming in incoming_entries {
        let id = incoming
            .base_id()
            .ok_or_else(|| Error::session("SQLite session entry is missing its ID"))?;
        let encoded = serde_json::to_string(incoming)?;
        if let Some(persisted) = by_id.get(id) {
            if persisted != &encoded {
                return Err(Error::session(format!(
                    "SQLite session entry ID {id} has conflicting persisted content"
                )));
            }
            continue;
        }
        by_id.insert(id.clone(), encoded.clone());
        entries.push(incoming.clone());
        json.push(encoded.clone());
        appended_json.push(encoded);
    }

    Ok(ReconciledEntries {
        entries,
        json,
        appended_json,
    })
}

fn validate_sqlite_sequence_range(start_seq: usize, entry_count: usize) -> Result<()> {
    let start = i64::try_from(start_seq)
        .map_err(|_| Error::session("SQLite start sequence exceeds i64"))?;
    let count =
        i64::try_from(entry_count).map_err(|_| Error::session("SQLite entry count exceeds i64"))?;
    start
        .checked_add(count)
        .ok_or_else(|| Error::session("SQLite session sequence range overflow"))?;
    Ok(())
}

fn insert_entry_jsons(
    conn: &SqliteConnection,
    json: &[String],
    existing_entry_count: usize,
) -> Result<()> {
    validate_sqlite_sequence_range(existing_entry_count, json.len())?;
    if json.is_empty() {
        return Ok(());
    }
    let mut seq = i64::try_from(existing_entry_count)
        .map_err(|_| Error::session("SQLite existing entry count exceeds i64"))?
        .checked_add(1)
        .ok_or_else(|| Error::session("SQLite session sequence overflow"))?;
    let mut remaining = json.len();
    for chunk in json.chunks(200) {
        let mut sql = String::with_capacity(64 + chunk.len() * 16);
        sql.push_str("INSERT INTO pi_session_entries (seq,json) VALUES ");
        let mut params = Vec::with_capacity(chunk.len() * 2);
        for (index, entry_json) in chunk.iter().enumerate() {
            if index > 0 {
                sql.push(',');
            }
            let _ = write!(sql, "(?{},?{})", index * 2 + 1, index * 2 + 2);
            params.push(SqliteValue::BigInt(seq));
            params.push(SqliteValue::Text(entry_json.clone()));
            remaining -= 1;
            if remaining > 0 {
                seq = seq
                    .checked_add(1)
                    .ok_or_else(|| Error::session("SQLite session sequence overflow"))?;
            }
        }
        map_sqlite_result(conn.execute_sync(&sql, &params))?;
    }
    Ok(())
}

fn write_session_meta(conn: &SqliteConnection, entries: &[SessionEntry]) -> Result<()> {
    let (message_count, name) = compute_message_count_and_name(entries);
    map_sqlite_result(conn.execute_sync(
        "INSERT OR REPLACE INTO pi_session_meta (key,value) VALUES (?1,?2)",
        &[
            SqliteValue::Text("message_count".to_string()),
            SqliteValue::Text(message_count.to_string()),
        ],
    ))?;
    map_sqlite_result(conn.execute_sync(
        "INSERT OR REPLACE INTO pi_session_meta (key,value) VALUES (?1,?2)",
        &[
            SqliteValue::Text("name".to_string()),
            SqliteValue::Text(name.unwrap_or_default()),
        ],
    ))?;
    Ok(())
}

#[allow(
    clippy::unused_async,
    reason = "session storage keeps an async backend contract"
)]
pub async fn load_session(path: &Path) -> Result<(SessionHeader, Vec<SessionEntry>)> {
    let metrics = session_metrics::global();
    let _timer = metrics.start_timer(&metrics.sqlite_load);

    if !crate::session::session_path_try_exists(path).map_err(|err| Error::Io(Box::new(err)))? {
        return Err(Error::SessionNotFound {
            path: path.display().to_string(),
        });
    }

    let conn = open_sqlite_connection_read_only(path)?;

    let header = read_stored_header(&conn)?
        .ok_or_else(|| Error::session("SQLite session missing header row"))?;

    let entries = read_all_entries(&conn)?;

    Ok((header, entries))
}

#[allow(
    clippy::unused_async,
    reason = "session storage keeps an async backend contract"
)]
pub async fn load_session_meta(path: &Path) -> Result<SqliteSessionMeta> {
    let metrics = session_metrics::global();
    let _timer = metrics.start_timer(&metrics.sqlite_load_meta);

    if !crate::session::session_path_try_exists(path).map_err(|err| Error::Io(Box::new(err)))? {
        return Err(Error::SessionNotFound {
            path: path.display().to_string(),
        });
    }

    let conn = open_sqlite_connection_read_only(path)?;

    let header = read_stored_header(&conn)?
        .ok_or_else(|| Error::session("SQLite session missing header row"))?;

    let meta_rows = query_session_meta_rows(&conn)?;

    let mut message_count: Option<u64> = None;
    let mut name: Option<String> = None;
    for row in meta_rows {
        let key = row_get_string(&row, "key")?;
        let value = row_get_string(&row, "value")?;
        match key.as_str() {
            "message_count" => message_count = value.parse::<u64>().ok(),
            "name" if !value.is_empty() => {
                name = Some(value);
            }
            _ => {}
        }
    }

    if message_count.is_none() || name.is_none() {
        let entries = read_all_entries(&conn)?;
        let (fallback_message_count, fallback_name) = compute_message_count_and_name(&entries);
        if message_count.is_none() {
            message_count = Some(fallback_message_count);
        }
        if name.is_none() {
            name = fallback_name;
        }
    }

    Ok(SqliteSessionMeta {
        header,
        message_count: message_count.unwrap_or(0),
        name,
    })
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;
    use crate::model::UserContent;
    use crate::session::{EntryBase, MessageEntry, SessionInfoEntry, SessionMessage};
    use std::sync::atomic::{AtomicU64, Ordering};

    #[cfg(unix)]
    struct UnixModeGuard {
        path: PathBuf,
        original: Option<std::fs::Permissions>,
    }

    #[cfg(unix)]
    impl UnixModeGuard {
        fn apply(path: &Path, mode: u32) -> Self {
            use std::os::unix::fs::PermissionsExt as _;

            let original = std::fs::metadata(path)
                .expect("permission fixture metadata")
                .permissions();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
                .expect("apply permission fixture mode");
            Self {
                path: path.to_path_buf(),
                original: Some(original),
            }
        }

        fn restore(&mut self) {
            if let Some(original) = self.original.as_ref() {
                std::fs::set_permissions(&self.path, original.clone())
                    .expect("restore permission fixture mode");
                self.original = None;
            }
        }
    }

    #[cfg(unix)]
    impl Drop for UnixModeGuard {
        fn drop(&mut self) {
            if let Some(original) = self.original.take() {
                let _ = std::fs::set_permissions(&self.path, original);
            }
        }
    }

    #[cfg(unix)]
    fn assert_permission_denied(error: &Error) {
        let kind = match error {
            Error::Io(io_error) => Some(io_error.kind()),
            _ => None,
        };
        assert_eq!(
            kind,
            Some(std::io::ErrorKind::PermissionDenied),
            "expected typed PermissionDenied error, got {error}"
        );
    }

    fn dummy_base() -> EntryBase {
        static NEXT_ENTRY_ID: AtomicU64 = AtomicU64::new(1);
        let entry_id = NEXT_ENTRY_ID.fetch_add(1, Ordering::Relaxed);
        EntryBase {
            id: Some(format!("test-id-{entry_id}")),
            parent_id: None,
            timestamp: "2026-01-01T00:00:00.000Z".to_string(),
        }
    }

    fn message_entry() -> SessionEntry {
        SessionEntry::Message(MessageEntry {
            base: dummy_base(),
            message: SessionMessage::User {
                content: UserContent::Text("hello".to_string()),
                timestamp: None,
            },
        })
    }

    fn message_entry_with_id(id: &str, text: &str) -> SessionEntry {
        SessionEntry::Message(MessageEntry {
            base: EntryBase {
                id: Some(id.to_string()),
                parent_id: None,
                timestamp: "2026-01-01T00:00:00.000Z".to_string(),
            },
            message: SessionMessage::User {
                content: UserContent::Text(text.to_string()),
                timestamp: None,
            },
        })
    }

    fn session_info_entry(name: Option<String>) -> SessionEntry {
        SessionEntry::SessionInfo(SessionInfoEntry {
            base: dummy_base(),
            name,
        })
    }

    #[test]
    fn compute_counts_empty() {
        let (count, name) = compute_message_count_and_name(&[]);
        assert_eq!(count, 0);
        assert!(name.is_none());
    }

    #[test]
    fn sqlite_json_write_limit_accepts_exact_cap_and_rejects_cap_plus_one() {
        validate_sqlite_json_length("session entry", MAX_SQLITE_JSON_BYTES)
            .expect("exact SQLite JSON cap must be accepted");
        let error = validate_sqlite_json_length("session entry", MAX_SQLITE_JSON_BYTES + 1)
            .expect_err("SQLite JSON cap plus one must be rejected");
        assert!(error.to_string().contains("exceeds JSON limit"));
    }

    #[test]
    fn compute_counts_messages_only() {
        let entries = vec![message_entry(), message_entry(), message_entry()];
        let (count, name) = compute_message_count_and_name(&entries);
        assert_eq!(count, 3);
        assert!(name.is_none());
    }

    #[test]
    fn compute_counts_session_info_with_name() {
        let entries = vec![
            message_entry(),
            session_info_entry(Some("My Session".to_string())),
            message_entry(),
        ];
        let (count, name) = compute_message_count_and_name(&entries);
        assert_eq!(count, 2);
        assert_eq!(name, Some("My Session".to_string()));
    }

    #[test]
    fn compute_counts_session_info_none_name_ignored() {
        let entries = vec![
            session_info_entry(Some("First".to_string())),
            session_info_entry(None),
            message_entry(),
        ];
        let (count, name) = compute_message_count_and_name(&entries);
        assert_eq!(count, 1);
        // The second SessionInfo has name=None, so it doesn't overwrite.
        assert_eq!(name, Some("First".to_string()));
    }

    #[test]
    fn compute_counts_latest_name_wins() {
        let entries = vec![
            session_info_entry(Some("First".to_string())),
            session_info_entry(Some("Second".to_string())),
        ];
        let (_, name) = compute_message_count_and_name(&entries);
        assert_eq!(name, Some("Second".to_string()));
    }

    // -- Non-message / non-session-info entries are ignored --

    #[test]
    fn compute_counts_ignores_model_change_entries() {
        use crate::session::ModelChangeEntry;
        let entries = vec![
            message_entry(),
            SessionEntry::ModelChange(ModelChangeEntry {
                base: dummy_base(),
                provider: "anthropic".to_string(),
                model_id: "claude-sonnet-4-5".to_string(),
            }),
            message_entry(),
        ];
        let (count, name) = compute_message_count_and_name(&entries);
        assert_eq!(count, 2);
        assert!(name.is_none());
    }

    #[test]
    fn compute_counts_ignores_label_entries() {
        use crate::session::LabelEntry;
        let entries = vec![
            message_entry(),
            SessionEntry::Label(LabelEntry {
                base: dummy_base(),
                target_id: "some-id".to_string(),
                label: Some("important".to_string()),
            }),
        ];
        let (count, name) = compute_message_count_and_name(&entries);
        assert_eq!(count, 1);
        assert!(name.is_none());
    }

    #[test]
    fn compute_counts_ignores_custom_entries() {
        use crate::session::CustomEntry;
        let entries = vec![
            SessionEntry::Custom(CustomEntry {
                base: dummy_base(),
                custom_type: "my_custom".to_string(),
                data: Some(serde_json::json!({"key": "value"})),
            }),
            message_entry(),
        ];
        let (count, name) = compute_message_count_and_name(&entries);
        assert_eq!(count, 1);
        assert!(name.is_none());
    }

    #[test]
    fn compute_counts_ignores_compaction_entries() {
        use crate::session::CompactionEntry;
        let entries = vec![
            message_entry(),
            SessionEntry::Compaction(CompactionEntry {
                base: dummy_base(),
                summary: "summary text".to_string(),
                first_kept_entry_id: "e1".to_string(),
                tokens_before: 500,
                details: None,
                from_hook: None,
            }),
            message_entry(),
            message_entry(),
        ];
        let (count, name) = compute_message_count_and_name(&entries);
        assert_eq!(count, 3);
        assert!(name.is_none());
    }

    #[test]
    fn compute_counts_mixed_entry_types() {
        use crate::session::{CompactionEntry, CustomEntry, LabelEntry, ModelChangeEntry};
        let entries = vec![
            message_entry(),
            SessionEntry::ModelChange(ModelChangeEntry {
                base: dummy_base(),
                provider: "openai".to_string(),
                model_id: "gpt-4".to_string(),
            }),
            session_info_entry(Some("Named".to_string())),
            SessionEntry::Label(LabelEntry {
                base: dummy_base(),
                target_id: "t1".to_string(),
                label: None,
            }),
            message_entry(),
            SessionEntry::Compaction(CompactionEntry {
                base: dummy_base(),
                summary: "s".to_string(),
                first_kept_entry_id: "e1".to_string(),
                tokens_before: 100,
                details: None,
                from_hook: None,
            }),
            SessionEntry::Custom(CustomEntry {
                base: dummy_base(),
                custom_type: "ct".to_string(),
                data: None,
            }),
            message_entry(),
        ];
        let (count, name) = compute_message_count_and_name(&entries);
        assert_eq!(count, 3);
        assert_eq!(name, Some("Named".to_string()));
    }

    // -- map_sqlite_result tests --

    #[test]
    fn map_sqlite_result_ok() {
        let result = map_sqlite_result::<i32>(Ok(42));
        assert_eq!(result.unwrap(), 42);
    }

    #[test]
    fn map_sqlite_result_err() {
        let config = SqliteConfig::file("bad\0path").flags(OpenFlags::create_read_write());
        let result = map_sqlite_result::<i32>(SqliteConnection::open(&config).map(|_| 42));
        let err = result.unwrap_err();
        match err {
            Error::Session(message) => {
                assert!(message.contains("SQLite session error"));
            }
            other => unreachable!("Unexpected error: {:?}", other),
        }
    }

    // -- SqliteSessionMeta struct --

    #[test]
    fn sqlite_session_meta_fields() {
        let meta = SqliteSessionMeta {
            header: SessionHeader {
                id: "test-session".to_string(),
                ..SessionHeader::default()
            },
            message_count: 42,
            name: Some("My Session".to_string()),
        };
        assert_eq!(meta.header.id, "test-session");
        assert_eq!(meta.message_count, 42);
        assert_eq!(meta.name.as_deref(), Some("My Session"));
    }

    #[test]
    fn sqlite_session_meta_no_name() {
        let meta = SqliteSessionMeta {
            header: SessionHeader::default(),
            message_count: 0,
            name: None,
        };
        assert_eq!(meta.message_count, 0);
        assert!(meta.name.is_none());
    }

    // -- compute_message_count_and_name: large input --

    #[test]
    fn compute_counts_large_message_set() {
        let entries: Vec<SessionEntry> = (0..1000).map(|_| message_entry()).collect();
        let (count, name) = compute_message_count_and_name(&entries);
        assert_eq!(count, 1000);
        assert!(name.is_none());
    }

    // -- compute_message_count_and_name: name then messages only --

    #[test]
    fn compute_counts_name_set_early_persists() {
        let entries = vec![
            session_info_entry(Some("Early Name".to_string())),
            message_entry(),
            message_entry(),
            message_entry(),
        ];
        let (count, name) = compute_message_count_and_name(&entries);
        assert_eq!(count, 3);
        assert_eq!(name, Some("Early Name".to_string()));
    }

    // -- compute_message_count_and_name: branch summary entry --

    #[test]
    fn compute_counts_ignores_branch_summary() {
        use crate::session::BranchSummaryEntry;
        let entries = vec![
            message_entry(),
            SessionEntry::BranchSummary(BranchSummaryEntry {
                base: dummy_base(),
                from_id: "parent-id".to_string(),
                summary: "branch summary".to_string(),
                details: None,
                from_hook: None,
            }),
        ];
        let (count, name) = compute_message_count_and_name(&entries);
        assert_eq!(count, 1);
        assert!(name.is_none());
    }

    // -- compute_message_count_and_name: thinking level change --

    #[test]
    fn compute_counts_ignores_thinking_level_change() {
        use crate::session::ThinkingLevelChangeEntry;
        let entries = vec![
            SessionEntry::ThinkingLevelChange(ThinkingLevelChangeEntry {
                base: dummy_base(),
                thinking_level: "high".to_string(),
            }),
            message_entry(),
        ];
        let (count, name) = compute_message_count_and_name(&entries);
        assert_eq!(count, 1);
        assert!(name.is_none());
    }

    #[test]
    fn save_session_rejects_semantically_invalid_header() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("invalid.sqlite");
        let header = SessionHeader {
            r#type: "note".to_string(),
            ..SessionHeader::default()
        };

        let err =
            futures::executor::block_on(async { save_session(&path, &header, &[], true).await })
                .expect_err("invalid header should fail");
        let message = err.to_string();
        assert!(
            message.contains("Invalid session header"),
            "expected invalid session header error, got {message}"
        );
    }

    #[test]
    fn load_session_meta_rejects_semantically_invalid_header() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("invalid.sqlite");
        let header = SessionHeader {
            id: "sqlite-test".to_string(),
            ..SessionHeader::default()
        };

        futures::executor::block_on(async { save_session(&path, &header, &[], true).await })
            .expect("save sqlite session");

        let invalid_header = SessionHeader {
            r#type: "note".to_string(),
            ..header
        };
        let invalid_json =
            serde_json::to_string(&invalid_header).expect("serialize invalid session header");
        let config = sqlmodel_sqlite::SqliteConfig::file(path.to_string_lossy())
            .flags(sqlmodel_sqlite::OpenFlags::create_read_write());
        let conn = sqlmodel_sqlite::SqliteConnection::open(&config).expect("open sqlite db");
        conn.execute_sync(
            "UPDATE pi_session_header SET json = ?1",
            &[sqlmodel_core::Value::Text(invalid_json)],
        )
        .expect("corrupt sqlite header row");

        let err = futures::executor::block_on(async { load_session_meta(&path).await })
            .expect_err("invalid header should fail");
        let message = err.to_string();
        assert!(
            message.contains("Invalid session header"),
            "expected invalid session header error, got {message}"
        );
    }

    #[test]
    fn load_session_meta_falls_back_to_entries_when_name_row_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("missing-name-row.sqlite");
        let header = SessionHeader {
            id: "sqlite-name-fallback".to_string(),
            ..SessionHeader::default()
        };
        let entries = vec![
            session_info_entry(Some("Recovered Name".to_string())),
            message_entry(),
            message_entry(),
        ];

        futures::executor::block_on(async { save_session(&path, &header, &entries, true).await })
            .expect("save sqlite session");

        let config = sqlmodel_sqlite::SqliteConfig::file(path.to_string_lossy())
            .flags(sqlmodel_sqlite::OpenFlags::create_read_write());
        let conn = sqlmodel_sqlite::SqliteConnection::open(&config).expect("open sqlite db");
        conn.execute_sync(
            "DELETE FROM pi_session_meta WHERE key = ?1",
            &[SqliteValue::Text("name".to_string())],
        )
        .expect("delete name meta row");

        let meta = futures::executor::block_on(async { load_session_meta(&path).await })
            .expect("load sqlite meta");
        assert_eq!(meta.message_count, 2);
        assert_eq!(meta.name.as_deref(), Some("Recovered Name"));
    }

    #[test]
    fn load_session_meta_falls_back_when_meta_table_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("missing-meta-table.sqlite");
        let header = SessionHeader {
            id: "sqlite-missing-meta".to_string(),
            ..SessionHeader::default()
        };
        let entries = vec![
            session_info_entry(Some("Recovered From Entries".to_string())),
            message_entry(),
        ];

        futures::executor::block_on(async { save_session(&path, &header, &entries, true).await })
            .expect("save sqlite session");

        let config = sqlmodel_sqlite::SqliteConfig::file(path.to_string_lossy())
            .flags(sqlmodel_sqlite::OpenFlags::create_read_write());
        let conn = sqlmodel_sqlite::SqliteConnection::open(&config).expect("open sqlite db");
        conn.execute_raw("DROP TABLE pi_session_meta")
            .expect("drop sqlite meta table");

        let meta = futures::executor::block_on(async { load_session_meta(&path).await })
            .expect("load sqlite meta");
        assert_eq!(meta.message_count, 1);
        assert_eq!(meta.name.as_deref(), Some("Recovered From Entries"));
    }

    #[test]
    fn load_session_meta_rejects_invalid_meta_table_schema() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("invalid-meta-schema.sqlite");
        let header = SessionHeader {
            id: "sqlite-invalid-meta-schema".to_string(),
            ..SessionHeader::default()
        };

        futures::executor::block_on(async {
            save_session(&path, &header, &[message_entry()], true).await
        })
        .expect("save sqlite session");

        let config = sqlmodel_sqlite::SqliteConfig::file(path.to_string_lossy())
            .flags(sqlmodel_sqlite::OpenFlags::create_read_write());
        let conn = sqlmodel_sqlite::SqliteConnection::open(&config).expect("open sqlite db");
        conn.execute_raw("DROP TABLE pi_session_meta")
            .expect("drop sqlite meta table");
        conn.execute_raw("CREATE TABLE pi_session_meta (key TEXT PRIMARY KEY)")
            .expect("create invalid sqlite meta table");

        let err = futures::executor::block_on(async { load_session_meta(&path).await })
            .expect_err("invalid meta schema should fail");
        let message = err.to_string();
        assert!(
            message.contains("SQLite session meta query failed"),
            "expected meta query error, got {message}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn load_paths_accept_read_only_sqlite_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("readonly.sqlite");
        let header = SessionHeader {
            id: "sqlite-readonly".to_string(),
            ..SessionHeader::default()
        };
        let entries = vec![
            session_info_entry(Some("Read Only".to_string())),
            message_entry(),
        ];

        futures::executor::block_on(async { save_session(&path, &header, &entries, true).await })
            .expect("save sqlite session");

        // Retained intentionally: this verifies that read-only load paths need
        // read permission only and never attempt to mutate the database.
        let mut mode_guard = UnixModeGuard::apply(&path, 0o444);

        let (loaded_header, loaded_entries) =
            futures::executor::block_on(async { load_session(&path).await })
                .expect("load readonly sqlite session");
        let meta = futures::executor::block_on(async { load_session_meta(&path).await })
            .expect("load readonly sqlite meta");

        assert_eq!(loaded_header.id, header.id);
        assert_eq!(loaded_entries.len(), entries.len());
        assert_eq!(meta.header.id, header.id);
        assert_eq!(meta.message_count, 1);
        assert_eq!(meta.name.as_deref(), Some("Read Only"));
        mode_guard.restore();
    }

    #[cfg(unix)]
    #[test]
    fn save_session_rejects_read_only_sqlite_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("readonly-save.sqlite");
        let original_header = SessionHeader {
            id: "sqlite-readonly-save".to_string(),
            ..SessionHeader::default()
        };
        let original_entries = vec![message_entry()];

        futures::executor::block_on(async {
            save_session(&path, &original_header, &original_entries, true).await
        })
        .expect("seed sqlite session");

        let mut mode_guard = UnixModeGuard::apply(&path, 0o444);
        let replacement_header = SessionHeader {
            id: "sqlite-replacement".to_string(),
            ..SessionHeader::default()
        };
        let result = futures::executor::block_on(async {
            save_session(
                &path,
                &replacement_header,
                &[message_entry(), message_entry()],
                true,
            )
            .await
        });
        mode_guard.restore();

        let error = result.expect_err("saving a mode-0444 SQLite session must fail");
        assert_permission_denied(&error);

        let (loaded_header, loaded_entries) =
            futures::executor::block_on(async { load_session(&path).await })
                .expect("reload original sqlite session");
        assert_eq!(loaded_header.id, original_header.id);
        assert_eq!(loaded_entries.len(), original_entries.len());
    }

    #[cfg(unix)]
    #[test]
    fn fresh_eyes_sqlite_save_rejects_unwritable_preexisting_sidecars_before_mutation() {
        for (suffix, sentinel) in [
            ("-wal", b"wal-sentinel".as_slice()),
            ("-shm", b"shm-sentinel".as_slice()),
            ("-journal", b"journal-sentinel".as_slice()),
        ] {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("guarded-sidecars.sqlite");
            let original_header = SessionHeader {
                id: format!("sqlite-sidecar-original-{suffix}"),
                ..SessionHeader::default()
            };
            futures::executor::block_on(async {
                save_session(&path, &original_header, &[message_entry()], true).await
            })
            .expect("seed sqlite session");
            let original_database = std::fs::read(&path).expect("read original database");
            let sidecar = append_sidecar_suffix(&path, suffix);
            std::fs::write(&sidecar, sentinel).expect("write guarded sidecar");
            let mut mode_guard = UnixModeGuard::apply(&sidecar, 0o466);
            let replacement_header = SessionHeader {
                id: format!("must-not-persist-{suffix}"),
                ..SessionHeader::default()
            };

            let result = futures::executor::block_on(async {
                save_session(
                    &path,
                    &replacement_header,
                    &[message_entry(), message_entry()],
                    true,
                )
                .await
            });
            let database_after = std::fs::read(&path).expect("read database after denial");
            let sidecar_after = std::fs::read(&sidecar).expect("read sidecar after denial");
            mode_guard.restore();

            let error = result.expect_err("selected owner class must deny sidecar mutation");
            assert_permission_denied(&error);
            assert!(
                error.to_string().contains(suffix),
                "denial must identify intended {suffix} sidecar: {error}"
            );
            assert_eq!(
                database_after, original_database,
                "database changed for {suffix}"
            );
            assert_eq!(sidecar_after, sentinel, "sidecar changed for {suffix}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn save_rejects_terminal_sqlite_symlink_without_changing_target() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("target.sqlite");
        let original_header = SessionHeader {
            id: "sqlite-symlink-target".to_string(),
            ..SessionHeader::default()
        };
        futures::executor::block_on(async {
            save_session(&target, &original_header, &[message_entry()], true).await
        })
        .expect("seed SQLite target");
        let original_database = std::fs::read(&target).expect("read SQLite target");
        let link = dir.path().join("linked.sqlite");
        symlink(&target, &link).expect("create SQLite symlink");

        let replacement_header = SessionHeader {
            id: "must-not-follow-main-symlink".to_string(),
            ..SessionHeader::default()
        };
        let result = futures::executor::block_on(async {
            save_session(
                &link,
                &replacement_header,
                &[message_entry(), message_entry()],
                true,
            )
            .await
        });

        let error = result.expect_err("SQLite persistence must reject a terminal symlink");
        assert!(
            matches!(&error, Error::Io(io_error) if io_error.kind() == std::io::ErrorKind::InvalidData),
            "expected typed InvalidData error, got {error}"
        );
        assert!(
            std::fs::symlink_metadata(&link)
                .expect("SQLite link metadata")
                .file_type()
                .is_symlink(),
            "rejected save must preserve the SQLite symlink"
        );
        assert_eq!(
            std::fs::read(&target).expect("read preserved SQLite target"),
            original_database,
            "rejected main-artifact symlink changed the target database"
        );
    }

    #[cfg(unix)]
    #[test]
    fn fresh_eyes_sqlite_save_rejects_sidecar_symlinks_without_touching_targets() {
        use std::os::unix::fs::symlink;

        for suffix in ["-wal", "-shm", "-journal"] {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("sidecar-link.sqlite");
            let original_header = SessionHeader {
                id: format!("sqlite-sidecar-link-{suffix}"),
                ..SessionHeader::default()
            };
            futures::executor::block_on(async {
                save_session(&path, &original_header, &[message_entry()], true).await
            })
            .expect("seed SQLite session");
            let original_database = std::fs::read(&path).expect("read SQLite database");

            let external_target = dir.path().join(format!("external{suffix}"));
            let sentinel = format!("external target for {suffix}");
            std::fs::write(&external_target, sentinel.as_bytes()).expect("write external target");
            let sidecar = append_sidecar_suffix(&path, suffix);
            symlink(&external_target, &sidecar).expect("create SQLite sidecar symlink");

            let replacement_header = SessionHeader {
                id: format!("must-not-follow-{suffix}"),
                ..SessionHeader::default()
            };
            let result = futures::executor::block_on(async {
                save_session(
                    &path,
                    &replacement_header,
                    &[message_entry(), message_entry()],
                    true,
                )
                .await
            });

            let error = result.expect_err("SQLite persistence must reject sidecar symlinks");
            assert!(
                matches!(&error, Error::Io(io_error) if io_error.kind() == std::io::ErrorKind::InvalidData),
                "expected typed InvalidData for {suffix}, got {error}"
            );
            assert!(
                std::fs::symlink_metadata(&sidecar)
                    .expect("sidecar link metadata")
                    .file_type()
                    .is_symlink(),
                "rejected save must preserve {suffix} symlink"
            );
            assert_eq!(
                std::fs::read(&external_target).expect("read external target"),
                sentinel.as_bytes(),
                "rejected {suffix} symlink changed its external target"
            );
            assert_eq!(
                std::fs::read(&path).expect("read preserved SQLite database"),
                original_database,
                "rejected {suffix} symlink changed the primary database"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn readonly_open_accepts_readable_existing_wal_and_shm() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("readonly-shm.sqlite");
        let header = SessionHeader {
            id: "sqlite-readonly-shm".to_string(),
            ..SessionHeader::default()
        };
        futures::executor::block_on(async {
            save_session(&path, &header, &[message_entry()], true).await
        })
        .expect("seed sqlite session");

        let writer = open_sqlite_connection_read_write(&path).expect("open WAL writer");
        map_sqlite_result(writer.execute_raw(INIT_SQL)).expect("initialize WAL writer");
        map_sqlite_result(
            writer.execute_raw("BEGIN IMMEDIATE; UPDATE pi_session_header SET id = id; COMMIT;"),
        )
        .expect("materialize WAL sidecars");

        let wal_path = append_sidecar_suffix(&path, "-wal");
        let shm_path = append_sidecar_suffix(&path, "-shm");
        assert!(
            wal_path.exists(),
            "WAL fixture must exist while writer is open"
        );
        assert!(
            shm_path.exists(),
            "SHM fixture must exist while writer is open"
        );
        let mut wal_mode_guard = UnixModeGuard::apply(&wal_path, 0o400);
        let mut shm_mode_guard = UnixModeGuard::apply(&shm_path, 0o400);
        let result = futures::executor::block_on(async { load_session(&path).await });
        wal_mode_guard.restore();
        shm_mode_guard.restore();
        drop(writer);

        let (loaded_header, loaded_entries) =
            result.expect("readable existing WAL and SHM must support a read-only open");
        assert_eq!(loaded_header.id, header.id);
        assert_eq!(loaded_entries.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn readonly_open_requires_writable_parent_unless_both_sidecars_exist() {
        for present_suffix in [None, Some("-wal"), Some("-shm")] {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("readonly-incomplete-sidecars.sqlite");
            let header = SessionHeader {
                id: format!("sqlite-readonly-incomplete-{present_suffix:?}"),
                ..SessionHeader::default()
            };
            futures::executor::block_on(async {
                save_session(&path, &header, &[message_entry()], true).await
            })
            .expect("seed sqlite session");
            let wal_path = append_sidecar_suffix(&path, "-wal");
            let shm_path = append_sidecar_suffix(&path, "-shm");
            assert!(
                !wal_path.exists() && !shm_path.exists(),
                "closed seed connection should leave no WAL/SHM sidecars"
            );
            if let Some(suffix) = present_suffix {
                std::fs::write(append_sidecar_suffix(&path, suffix), b"sidecar-sentinel")
                    .expect("write partial sidecar fixture");
            }

            let mut mode_guard = UnixModeGuard::apply(dir.path(), 0o577);
            let result = futures::executor::block_on(async { load_session(&path).await });
            mode_guard.restore();

            let error = result
                .expect_err("an absent WAL or SHM sidecar requires a writable parent directory");
            assert_permission_denied(&error);
            assert!(path.exists(), "denied open must preserve the database");
            if let Some(suffix) = present_suffix {
                assert_eq!(
                    std::fs::read(append_sidecar_suffix(&path, suffix))
                        .expect("read preserved partial sidecar"),
                    b"sidecar-sentinel"
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn readonly_rollback_journal_database_does_not_require_writable_parent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("readonly-rollback.sqlite");
        let header = SessionHeader {
            id: "sqlite-readonly-rollback".to_string(),
            ..SessionHeader::default()
        };
        futures::executor::block_on(async {
            save_session(&path, &header, &[message_entry()], true).await
        })
        .expect("seed SQLite session");
        let connection = open_sqlite_connection_read_write(&path).expect("open SQLite session");
        map_sqlite_result(connection.execute_raw("PRAGMA journal_mode = DELETE;"))
            .expect("switch fixture to rollback-journal mode");
        drop(connection);
        assert!(
            !sqlite_database_uses_wal(&path).expect("inspect SQLite header"),
            "fixture must advertise rollback-journal mode in its database header"
        );

        let mut mode_guard = UnixModeGuard::apply(dir.path(), 0o577);
        let result = futures::executor::block_on(async { load_session(&path).await });
        mode_guard.restore();

        let (loaded_header, loaded_entries) =
            result.expect("rollback-journal read-only open must not create WAL sidecars");
        assert_eq!(loaded_header.id, header.id);
        assert_eq!(loaded_entries.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn save_session_sets_private_permissions_for_sqlite_artifacts() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("secure.sqlite");
        let header = SessionHeader {
            id: "sqlite-secure".to_string(),
            ..SessionHeader::default()
        };

        futures::executor::block_on(async {
            save_session(&path, &header, &[message_entry()], true).await
        })
        .expect("save sqlite session");
        let rollback_journal = append_sidecar_suffix(&path, "-journal");
        std::fs::write(&rollback_journal, b"rollback journal fixture")
            .expect("write rollback journal fixture");
        ensure_private_sqlite_permissions(&path).expect("secure rollback journal fixture");

        for artifact in sqlite_artifact_paths(&path) {
            if artifact.exists() {
                let mode = std::fs::metadata(&artifact)
                    .expect("sqlite artifact metadata")
                    .permissions()
                    .mode()
                    & 0o777;
                assert_eq!(
                    mode,
                    0o600,
                    "expected private permissions for {}",
                    artifact.display()
                );
            }
        }
    }

    #[test]
    fn stale_full_saves_merge_entries_without_lost_updates() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("stale-save.sqlite");
        let header = SessionHeader {
            id: "stale-save".to_string(),
            ..SessionHeader::default()
        };
        let first = message_entry_with_id("entry-a", "a");
        let second = message_entry_with_id("entry-b", "b");
        let third = message_entry_with_id("entry-c", "c");

        futures::executor::block_on(async {
            save_session(&path, &header, std::slice::from_ref(&first), true).await
        })
        .expect("seed SQLite snapshot");
        futures::executor::block_on(async {
            save_session(&path, &header, &[first.clone(), second.clone()], true).await
        })
        .expect("save first stale descendant");
        futures::executor::block_on(async {
            save_session(&path, &header, &[first.clone(), third.clone()], true).await
        })
        .expect("merge second stale descendant");

        let (_, entries) = futures::executor::block_on(async { load_session(&path).await })
            .expect("load merged SQLite session");
        let ids: Vec<_> = entries
            .iter()
            .map(|entry| entry.base_id().expect("entry id"))
            .collect();
        assert_eq!(ids, ["entry-a", "entry-b", "entry-c"]);
        let meta = futures::executor::block_on(async { load_session_meta(&path).await })
            .expect("load reconciled meta");
        assert_eq!(meta.message_count, 3);
    }

    #[test]
    fn full_save_rejects_identical_duplicate_local_ids_before_creating_sqlite() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("duplicate-full-save.sqlite");
        let header = SessionHeader {
            id: "duplicate-full-save".to_string(),
            ..SessionHeader::default()
        };
        let duplicate = message_entry_with_id("entry-duplicate", "same payload");

        let error = futures::executor::block_on(async {
            save_session(&path, &header, &[duplicate.clone(), duplicate], true).await
        })
        .expect_err("duplicate in-memory IDs must fail closed");

        assert!(
            error
                .to_string()
                .contains("in-memory SQLite session contains duplicate entry ID entry-duplicate"),
            "unexpected duplicate-ID diagnostic: {error}"
        );
        assert!(
            !path.exists(),
            "duplicate-ID validation must precede SQLite creation"
        );
    }

    #[test]
    fn full_save_rejects_different_session_id_without_mutation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("identity-guard.sqlite");
        let original_header = SessionHeader {
            id: "sqlite-session-a".to_string(),
            ..SessionHeader::default()
        };
        let original_entry = message_entry_with_id("entry-a", "persisted");
        futures::executor::block_on(async {
            save_session(
                &path,
                &original_header,
                std::slice::from_ref(&original_entry),
                true,
            )
            .await
        })
        .expect("seed SQLite session");
        let different_header = SessionHeader {
            id: "sqlite-session-b".to_string(),
            ..original_header.clone()
        };

        let error = futures::executor::block_on(async {
            save_session(
                &path,
                &different_header,
                &[message_entry_with_id("entry-b", "must not persist")],
                true,
            )
            .await
        })
        .expect_err("different session identity must fail closed");
        let (loaded_header, loaded_entries) =
            futures::executor::block_on(async { load_session(&path).await })
                .expect("reload guarded SQLite session");

        assert!(error.to_string().contains("header ID"), "{error}");
        assert_eq!(loaded_header.id, original_header.id);
        assert_eq!(
            serde_json::to_value(loaded_entries).expect("serialize loaded entries"),
            serde_json::to_value([original_entry]).expect("serialize expected entries")
        );
    }

    #[test]
    fn stale_clean_full_save_preserves_newer_sqlite_header() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("header-intent.sqlite");
        let original_header = SessionHeader {
            id: "sqlite-header-intent".to_string(),
            ..SessionHeader::default()
        };
        futures::executor::block_on(async {
            save_session(&path, &original_header, &[], true).await
        })
        .expect("seed SQLite session");
        let mut newer_header = original_header.clone();
        newer_header.provider = Some("newer-provider".to_string());
        let (saved_header, _) = futures::executor::block_on(async {
            save_session(&path, &newer_header, &[], true).await
        })
        .expect("persist explicit same-session header update");
        assert_eq!(saved_header.provider.as_deref(), Some("newer-provider"));

        let (adopted_header, _) = futures::executor::block_on(async {
            save_session(&path, &original_header, &[], false).await
        })
        .expect("stale clean save must adopt the disk header");
        let (reloaded_header, _) = futures::executor::block_on(async { load_session(&path).await })
            .expect("reload SQLite session");

        assert_eq!(adopted_header.provider.as_deref(), Some("newer-provider"));
        assert_eq!(reloaded_header.provider.as_deref(), Some("newer-provider"));
    }

    #[test]
    fn stale_incremental_appends_merge_and_recompute_meta() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("stale-append.sqlite");
        let header = SessionHeader {
            id: "stale-append".to_string(),
            ..SessionHeader::default()
        };
        let first = message_entry_with_id("entry-a", "a");
        let second = message_entry_with_id("entry-b", "b");
        let third = message_entry_with_id("entry-c", "c");
        futures::executor::block_on(async { save_session(&path, &header, &[first], true).await })
            .expect("seed SQLite session");

        futures::executor::block_on(async {
            append_entries(&path, &header.id, &[second], 1).await
        })
        .expect("append first writer");
        futures::executor::block_on(async { append_entries(&path, &header.id, &[third], 1).await })
            .expect("merge stale second writer");

        let (_, entries) = futures::executor::block_on(async { load_session(&path).await })
            .expect("load merged SQLite session");
        let ids: Vec<_> = entries
            .iter()
            .map(|entry| entry.base_id().expect("entry id"))
            .collect();
        assert_eq!(ids, ["entry-a", "entry-b", "entry-c"]);
        let meta = futures::executor::block_on(async { load_session_meta(&path).await })
            .expect("load recomputed SQLite meta");
        assert_eq!(meta.message_count, 3);
        assert!(meta.name.is_none());
    }

    #[test]
    fn incremental_append_rejects_identical_duplicate_local_ids_without_mutation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("duplicate-append.sqlite");
        let header = SessionHeader {
            id: "duplicate-append".to_string(),
            ..SessionHeader::default()
        };
        let original = message_entry_with_id("entry-a", "persisted");
        futures::executor::block_on(async {
            save_session(&path, &header, std::slice::from_ref(&original), true).await
        })
        .expect("seed SQLite session");
        let duplicate = message_entry_with_id("entry-b", "same pending payload");

        let error = futures::executor::block_on(async {
            append_entries(&path, &header.id, &[duplicate.clone(), duplicate], 1).await
        })
        .expect_err("duplicate incremental IDs must fail closed");
        let (loaded_header, loaded_entries) =
            futures::executor::block_on(async { load_session(&path).await })
                .expect("reload preserved SQLite session");

        assert!(
            error
                .to_string()
                .contains("incremental SQLite session append contains duplicate entry ID entry-b"),
            "unexpected duplicate-ID diagnostic: {error}"
        );
        assert_eq!(loaded_header.id, header.id);
        assert_eq!(
            serde_json::to_value(loaded_entries).expect("serialize loaded entries"),
            serde_json::to_value([original]).expect("serialize expected entries")
        );
    }

    #[test]
    fn incremental_append_rejects_different_session_id_without_mutation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("append-identity-guard.sqlite");
        let header = SessionHeader {
            id: "sqlite-append-identity".to_string(),
            ..SessionHeader::default()
        };
        let original = message_entry_with_id("entry-a", "persisted");
        futures::executor::block_on(async {
            save_session(&path, &header, std::slice::from_ref(&original), true).await
        })
        .expect("seed SQLite session");

        let error = futures::executor::block_on(async {
            append_entries(
                &path,
                "different-session-id",
                &[message_entry_with_id("entry-b", "must not persist")],
                1,
            )
            .await
        })
        .expect_err("incremental append must bind to the expected session identity");
        let (_, loaded_entries) = futures::executor::block_on(async { load_session(&path).await })
            .expect("reload guarded SQLite session");

        assert!(error.to_string().contains("header ID"), "{error}");
        assert_eq!(
            serde_json::to_value(loaded_entries).expect("serialize loaded entries"),
            serde_json::to_value([original]).expect("serialize expected entries")
        );
    }

    #[test]
    fn conflicting_stale_entry_is_rejected_without_mutation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("conflicting-entry.sqlite");
        let header = SessionHeader {
            id: "conflicting-entry".to_string(),
            ..SessionHeader::default()
        };
        let original = message_entry_with_id("entry-a", "original");
        futures::executor::block_on(async {
            save_session(&path, &header, std::slice::from_ref(&original), true).await
        })
        .expect("seed SQLite session");

        let conflicting = message_entry_with_id("entry-a", "conflicting secret payload");
        let error = futures::executor::block_on(async {
            append_entries(&path, &header.id, &[conflicting], 1).await
        })
        .expect_err("conflicting entry ID must fail");
        assert!(error.to_string().contains("conflicting persisted content"));

        let (_, entries) = futures::executor::block_on(async { load_session(&path).await })
            .expect("load preserved SQLite session");
        assert_eq!(
            serde_json::to_value(&entries).expect("serialize persisted entries"),
            serde_json::to_value([original]).expect("serialize expected entries")
        );
    }

    #[test]
    fn full_save_rejects_missing_parent_without_mutation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("missing-parent.sqlite");
        let header = SessionHeader {
            id: "sqlite-missing-parent".to_string(),
            ..SessionHeader::default()
        };
        let original = message_entry_with_id("entry-a", "persisted");
        futures::executor::block_on(async {
            save_session(&path, &header, std::slice::from_ref(&original), true).await
        })
        .expect("seed SQLite session");

        let mut orphan = message_entry_with_id("entry-orphan", "must not persist");
        orphan.base_mut().parent_id = Some("missing-parent".to_string());
        let error = futures::executor::block_on(async {
            save_session(&path, &header, &[original.clone(), orphan], true).await
        })
        .expect_err("a missing parent must reject the reconciled graph");

        assert!(error.to_string().contains("references missing parent"));
        let (loaded_header, loaded_entries) =
            futures::executor::block_on(async { load_session(&path).await })
                .expect("load preserved SQLite session");
        assert_eq!(
            serde_json::to_value(loaded_header).expect("serialize loaded header"),
            serde_json::to_value(header).expect("serialize expected header")
        );
        assert_eq!(
            serde_json::to_value(loaded_entries).expect("serialize loaded entries"),
            serde_json::to_value([original]).expect("serialize expected entries")
        );
    }

    #[test]
    fn incremental_append_rejects_parent_cycle_without_mutation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("parent-cycle.sqlite");
        let header = SessionHeader {
            id: "sqlite-parent-cycle".to_string(),
            ..SessionHeader::default()
        };
        let original = message_entry_with_id("entry-root", "persisted");
        futures::executor::block_on(async {
            save_session(&path, &header, std::slice::from_ref(&original), true).await
        })
        .expect("seed SQLite session");

        let mut cycle_a = message_entry_with_id("cycle-a", "must not persist a");
        cycle_a.base_mut().parent_id = Some("cycle-b".to_string());
        let mut cycle_b = message_entry_with_id("cycle-b", "must not persist b");
        cycle_b.base_mut().parent_id = Some("cycle-a".to_string());
        let error = futures::executor::block_on(async {
            append_entries(&path, &header.id, &[cycle_a, cycle_b], 1).await
        })
        .expect_err("a parent cycle must reject the reconciled graph");

        assert!(error.to_string().contains("parent graph contains a cycle"));
        let (loaded_header, loaded_entries) =
            futures::executor::block_on(async { load_session(&path).await })
                .expect("load preserved SQLite session");
        assert_eq!(
            serde_json::to_value(loaded_header).expect("serialize loaded header"),
            serde_json::to_value(header).expect("serialize expected header")
        );
        assert_eq!(
            serde_json::to_value(loaded_entries).expect("serialize loaded entries"),
            serde_json::to_value([original]).expect("serialize expected entries")
        );
    }

    #[test]
    fn append_sequence_range_and_ahead_snapshot_fail_closed() {
        #[cfg(target_pointer_width = "64")]
        {
            let sqlite_integer_max =
                usize::try_from(i64::MAX).expect("i64::MAX fits a 64-bit usize");
            validate_sqlite_sequence_range(sqlite_integer_max, 0)
                .expect("exact SQLite sequence ceiling with no append is valid");
            validate_sqlite_sequence_range(sqlite_integer_max, 1)
                .expect_err("SQLite sequence ceiling plus one must overflow");
            validate_sqlite_sequence_range(usize::MAX, 1)
                .expect_err("usize range beyond SQLite INTEGER must fail");
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ahead-snapshot.sqlite");
        let header = SessionHeader {
            id: "ahead-snapshot".to_string(),
            ..SessionHeader::default()
        };
        let first = message_entry_with_id("entry-a", "a");
        futures::executor::block_on(async {
            save_session(&path, &header, std::slice::from_ref(&first), true).await
        })
        .expect("seed SQLite session");
        let error = futures::executor::block_on(async {
            append_entries(
                &path,
                &header.id,
                &[message_entry_with_id("entry-b", "b")],
                2,
            )
            .await
        })
        .expect_err("snapshot ahead of persisted count must fail");
        assert!(error.to_string().contains("ahead of persisted state"));
        let (_, entries) = futures::executor::block_on(async { load_session(&path).await })
            .expect("load preserved session");
        assert_eq!(
            serde_json::to_value(&entries).expect("serialize persisted entries"),
            serde_json::to_value([first]).expect("serialize expected entries")
        );
    }

    #[test]
    fn malformed_sqlite_json_diagnostics_are_redacted_and_bounded() {
        const SENTINEL: &str = "DO_NOT_ECHO_THIS_SQLITE_SECRET";
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("redacted-json.sqlite");
        let header = SessionHeader {
            id: "redacted-json".to_string(),
            ..SessionHeader::default()
        };
        futures::executor::block_on(async {
            save_session(
                &path,
                &header,
                &[message_entry_with_id("entry-a", "a")],
                true,
            )
            .await
        })
        .expect("seed SQLite session");
        let malformed = format!("{{\"secret\":\"{SENTINEL}\",");
        let conn = open_sqlite_connection_read_write(&path).expect("open SQLite fixture");
        map_sqlite_result(conn.execute_sync(
            "UPDATE pi_session_entries SET json = ?1 WHERE seq = 1",
            &[SqliteValue::Text(malformed.clone())],
        ))
        .expect("write malformed entry JSON");
        drop(conn);

        let error = futures::executor::block_on(async { load_session(&path).await })
            .expect_err("malformed entry JSON must fail");
        let diagnostic = error.to_string();
        assert!(!diagnostic.contains(SENTINEL));
        assert!(diagnostic.contains(&format!("bytes={}", malformed.len())));
        assert!(diagnostic.contains("sha256="));
        assert!(diagnostic.contains("line="));
        assert!(diagnostic.contains("column="));

        let header_path = dir.path().join("redacted-header.sqlite");
        futures::executor::block_on(async { save_session(&header_path, &header, &[], true).await })
            .expect("seed header redaction session");
        let header_conn =
            open_sqlite_connection_read_write(&header_path).expect("open header fixture");
        map_sqlite_result(header_conn.execute_sync(
            "UPDATE pi_session_header SET json = ?1",
            &[SqliteValue::Text(malformed.clone())],
        ))
        .expect("write malformed header JSON");
        drop(header_conn);
        let header_error =
            futures::executor::block_on(async { load_session_meta(&header_path).await })
                .expect_err("malformed header JSON must fail");
        let header_diagnostic = header_error.to_string();
        assert!(!header_diagnostic.contains(SENTINEL));
        assert!(header_diagnostic.contains(&format!("bytes={}", malformed.len())));
        assert!(header_diagnostic.contains("sha256="));
        assert!(header_diagnostic.contains("line="));
        assert!(header_diagnostic.contains("column="));
    }
}

pub async fn save_session(
    path: &Path,
    header: &SessionHeader,
    entries: &[SessionEntry],
    header_dirty: bool,
) -> Result<(SessionHeader, Vec<SessionEntry>)> {
    header
        .validate()
        .map_err(|reason| Error::session(format!("Invalid session header: {reason}")))?;
    validate_unique_incoming_entry_ids(entries, "in-memory SQLite session")?;
    validate_sqlite_sequence_range(0, entries.len())?;
    let metrics = session_metrics::global();
    let _save_timer = metrics.start_timer(&metrics.sqlite_save);

    if let Some(parent) = path.parent() {
        let parent_to_check = parent.to_path_buf();
        asupersync::runtime::spawn_blocking(move || {
            crate::session::ensure_session_directory_creation_access(&parent_to_check)
                .map_err(|err| Error::Io(Box::new(err)))
        })
        .await?;
        asupersync::fs::create_dir_all(parent).await?;
    }

    let _lock = crate::session::lock_session_persistence(path)?;
    let conn = open_sqlite_connection_read_write(path)?;
    map_sqlite_result(conn.execute_raw(INIT_SQL))?;
    ensure_private_sqlite_permissions(path)?;
    map_sqlite_result(conn.execute_raw("BEGIN IMMEDIATE"))?;

    // Serialize header + entries and track serialization time + bytes.
    let save_result = (|| -> Result<(SessionHeader, Vec<SessionEntry>)> {
        let serialize_timer = metrics.start_timer(&metrics.sqlite_serialize);
        let header_to_write = match read_stored_header(&conn)? {
            Some(stored_header) => {
                if stored_header.id != header.id {
                    return Err(Error::session(
                        "SQLite session header ID does not match the in-memory session ID",
                    ));
                }
                if header_dirty {
                    header.clone()
                } else {
                    stored_header
                }
            }
            None => header.clone(),
        };
        let header_json = serde_json::to_string(&header_to_write)?;
        let reconciled = reconcile_entries(read_stored_entries(&conn)?, entries)?;
        crate::session::validate_session_entry_graph(&reconciled.entries)?;
        validate_sqlite_json_for_write("session header", &header_json)?;
        for entry_json in &reconciled.json {
            validate_sqlite_json_for_write("session entry", entry_json)?;
        }
        validate_sqlite_sequence_range(0, reconciled.json.len())?;
        let mut total_json_bytes = u64::try_from(header_json.len())
            .map_err(|_| Error::session("SQLite header JSON length exceeds u64"))?;
        for entry_json in &reconciled.json {
            total_json_bytes = total_json_bytes
                .checked_add(
                    u64::try_from(entry_json.len())
                        .map_err(|_| Error::session("SQLite entry JSON length exceeds u64"))?,
                )
                .ok_or_else(|| Error::session("SQLite serialized byte count overflow"))?;
        }
        serialize_timer.finish();
        metrics.record_bytes(&metrics.sqlite_bytes, total_json_bytes);

        map_sqlite_result(conn.execute_sync("DELETE FROM pi_session_entries", &[]))?;
        map_sqlite_result(conn.execute_sync("DELETE FROM pi_session_header", &[]))?;
        map_sqlite_result(conn.execute_sync("DELETE FROM pi_session_meta", &[]))?;

        map_sqlite_result(conn.execute_sync(
            "INSERT INTO pi_session_header (id,json) VALUES (?1,?2)",
            &[
                SqliteValue::Text(header_to_write.id.clone()),
                SqliteValue::Text(header_json),
            ],
        ))?;
        insert_entry_jsons(&conn, &reconciled.json, 0)?;
        write_session_meta(&conn, &reconciled.entries)?;

        Ok((header_to_write, reconciled.entries))
    })();

    match save_result {
        Ok((saved_header, saved_entries)) => {
            map_sqlite_result(conn.execute_raw("COMMIT"))?;
            ensure_private_sqlite_permissions(path)?;
            Ok((saved_header, saved_entries))
        }
        Err(err) => {
            rollback_quietly(&conn);
            Err(err)
        }
    }
}

/// Incrementally append new entries to an existing SQLite session database.
///
/// Only the entries in `new_entries` (starting at 1-based sequence `start_seq`)
/// are inserted. The header row is left unchanged, while the `message_count`
/// and `name` meta rows are upserted to reflect the current totals.
///
/// This avoids the DELETE+reinsert cost of [`save_session`] for the common
/// case where a few entries are appended between saves.
#[allow(
    clippy::unused_async,
    reason = "session storage keeps an async backend contract"
)]
pub async fn append_entries(
    path: &Path,
    expected_session_id: &str,
    new_entries: &[SessionEntry],
    start_seq: usize,
) -> Result<(SessionHeader, Vec<SessionEntry>)> {
    validate_unique_incoming_entry_ids(new_entries, "incremental SQLite session append")?;
    validate_sqlite_sequence_range(start_seq, new_entries.len())?;
    let metrics = session_metrics::global();
    let _timer = metrics.start_timer(&metrics.sqlite_append);

    let _lock = crate::session::lock_session_persistence(path)?;
    let conn = open_sqlite_connection_read_write(path)?;

    // Ensure WAL mode is active and tables exist (especially pi_session_meta for old DBs).
    map_sqlite_result(conn.execute_raw(INIT_SQL))?;
    ensure_private_sqlite_permissions(path)?;
    map_sqlite_result(conn.execute_raw("BEGIN IMMEDIATE"))?;

    let append_result = (|| -> Result<(SessionHeader, Vec<SessionEntry>)> {
        let serialize_timer = metrics.start_timer(&metrics.sqlite_serialize);
        let stored_header = read_stored_header(&conn)?
            .ok_or_else(|| Error::session("SQLite session missing header row"))?;
        if stored_header.id != expected_session_id {
            return Err(Error::session(
                "SQLite session header ID does not match the in-memory session ID",
            ));
        }
        let stored_entries = read_stored_entries(&conn)?;
        let existing_entry_count = stored_entries.len();
        if start_seq > existing_entry_count {
            return Err(Error::session(format!(
                "SQLite append snapshot is ahead of persisted state: start={start_seq} persisted={existing_entry_count}"
            )));
        }
        let reconciled = reconcile_entries(stored_entries, new_entries)?;
        crate::session::validate_session_entry_graph(&reconciled.entries)?;
        for entry_json in &reconciled.json {
            validate_sqlite_json_for_write("session entry", entry_json)?;
        }
        validate_sqlite_sequence_range(existing_entry_count, reconciled.appended_json.len())?;
        let mut total_json_bytes = 0u64;
        for entry_json in &reconciled.appended_json {
            total_json_bytes = total_json_bytes
                .checked_add(
                    u64::try_from(entry_json.len())
                        .map_err(|_| Error::session("SQLite entry JSON length exceeds u64"))?,
                )
                .ok_or_else(|| Error::session("SQLite serialized byte count overflow"))?;
        }
        serialize_timer.finish();
        metrics.record_bytes(&metrics.sqlite_bytes, total_json_bytes);

        insert_entry_jsons(&conn, &reconciled.appended_json, existing_entry_count)?;
        write_session_meta(&conn, &reconciled.entries)?;

        Ok((stored_header, reconciled.entries))
    })();

    match append_result {
        Ok((saved_header, saved_entries)) => {
            map_sqlite_result(conn.execute_raw("COMMIT"))?;
            ensure_private_sqlite_permissions(path)?;
            Ok((saved_header, saved_entries))
        }
        Err(err) => {
            rollback_quietly(&conn);
            Err(err)
        }
    }
}
