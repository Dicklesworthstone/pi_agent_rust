//! SQLite session index (derived from JSONL sessions).

use crate::config::Config;
use crate::error::{Error, Result};
use crate::session::{Session, SessionEntry, SessionHeader};
use fs4::fs_std::FileExt;
use sqlmodel_core::Value;
use sqlmodel_sqlite::{OpenFlags, SqliteConfig, SqliteConnection};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
pub struct SessionMeta {
    pub path: String,
    pub id: String,
    pub cwd: String,
    pub timestamp: String,
    pub message_count: u64,
    pub last_modified_ms: i64,
    pub size_bytes: u64,
    pub name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SessionIndex {
    db_path: PathBuf,
    lock_path: PathBuf,
}

impl SessionIndex {
    pub fn new() -> Self {
        let root = Config::sessions_dir();
        Self::for_sessions_root(&root)
    }

    pub fn for_sessions_root(root: &Path) -> Self {
        Self {
            db_path: root.join("session-index.sqlite"),
            lock_path: root.join("session-index.lock"),
        }
    }

    pub fn index_session(&self, session: &Session) -> Result<()> {
        let Some(path) = session.path.as_ref() else {
            return Ok(());
        };

        let meta = build_meta(path, &session.header, &session.entries)?;
        self.with_lock(|conn| {
            init_schema(conn)?;
            conn.execute_sync(
                "INSERT INTO sessions (path,id,cwd,timestamp,message_count,last_modified_ms,size_bytes,name)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8)
                 ON CONFLICT(path) DO UPDATE SET
                   id=excluded.id,
                   cwd=excluded.cwd,
                   timestamp=excluded.timestamp,
                   message_count=excluded.message_count,
                   last_modified_ms=excluded.last_modified_ms,
                   size_bytes=excluded.size_bytes,
                   name=excluded.name",
                &[
                    Value::Text(meta.path),
                    Value::Text(meta.id),
                    Value::Text(meta.cwd),
                    Value::Text(meta.timestamp),
                    Value::BigInt(i64::try_from(meta.message_count).unwrap_or(i64::MAX)),
                    Value::BigInt(meta.last_modified_ms),
                    Value::BigInt(i64::try_from(meta.size_bytes).unwrap_or(i64::MAX)),
                    meta.name.map_or(Value::Null, Value::Text),
                ],
            ).map_err(|e| Error::session(format!("Insert failed: {e}")))?;

            conn.execute_sync(
                "INSERT INTO meta (key,value) VALUES ('last_sync_epoch_ms', ?1)
                 ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                &[Value::Text(current_epoch_ms())],
            ).map_err(|e| Error::session(format!("Meta update failed: {e}")))?;
            Ok(())
        })
    }

    pub fn list_sessions(&self, cwd: Option<&str>) -> Result<Vec<SessionMeta>> {
        self.with_lock(|conn| {
            init_schema(conn)?;

            let (sql, params): (&str, Vec<Value>) = cwd.map_or_else(
                || {
                    (
                        "SELECT path,id,cwd,timestamp,message_count,last_modified_ms,size_bytes,name
                         FROM sessions ORDER BY last_modified_ms DESC",
                        vec![],
                    )
                },
                |cwd| {
                    (
                        "SELECT path,id,cwd,timestamp,message_count,last_modified_ms,size_bytes,name
                         FROM sessions WHERE cwd=?1 ORDER BY last_modified_ms DESC",
                        vec![Value::Text(cwd.to_string())],
                    )
                },
            );

            let rows = conn
                .query_sync(sql, &params)
                .map_err(|e| Error::session(format!("Query failed: {e}")))?;

            let mut result = Vec::new();
            for row in rows {
                result.push(row_to_meta(&row)?);
            }
            Ok(result)
        })
    }

    pub fn delete_session_path(&self, path: &Path) -> Result<()> {
        let path = path.to_string_lossy().to_string();
        self.with_lock(|conn| {
            init_schema(conn)?;
            conn.execute_sync("DELETE FROM sessions WHERE path=?1", &[Value::Text(path)])
                .map_err(|e| Error::session(format!("Delete failed: {e}")))?;
            Ok(())
        })
    }

    pub fn reindex_all(&self) -> Result<()> {
        let sessions_root = self.sessions_root();
        if !sessions_root.exists() {
            return Ok(());
        }

        let mut metas = Vec::new();
        for entry in walk_jsonl(sessions_root) {
            let Ok(path) = entry else { continue };
            if let Ok(meta) = build_meta_from_file(&path) {
                metas.push(meta);
            }
        }

        self.with_lock(|conn| {
            init_schema(conn)?;
            conn.execute_sync("DELETE FROM sessions", &[])
                .map_err(|e| Error::session(format!("Delete failed: {e}")))?;

            for meta in metas {
                conn.execute_sync(
                    "INSERT INTO sessions (path,id,cwd,timestamp,message_count,last_modified_ms,size_bytes,name)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                    &[
                        Value::Text(meta.path),
                        Value::Text(meta.id),
                        Value::Text(meta.cwd),
                        Value::Text(meta.timestamp),
                        Value::BigInt(i64::try_from(meta.message_count).unwrap_or(i64::MAX)),
                        Value::BigInt(meta.last_modified_ms),
                        Value::BigInt(i64::try_from(meta.size_bytes).unwrap_or(i64::MAX)),
                        meta.name.map_or(Value::Null, Value::Text),
                    ],
                ).map_err(|e| Error::session(format!("Insert failed: {e}")))?;
            }

            conn.execute_sync(
                "INSERT INTO meta (key,value) VALUES ('last_sync_epoch_ms', ?1)
                 ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                &[Value::Text(current_epoch_ms())],
            ).map_err(|e| Error::session(format!("Meta update failed: {e}")))?;
            Ok(())
        })
    }

    /// Check whether the on-disk index is stale enough to reindex.
    pub fn should_reindex(&self, max_age: Duration) -> bool {
        if !self.db_path.exists() {
            return true;
        }
        let Ok(meta) = fs::metadata(&self.db_path) else {
            return true;
        };
        let Ok(modified) = meta.modified() else {
            return true;
        };
        let age = SystemTime::now()
            .duration_since(modified)
            .unwrap_or_default();
        age > max_age
    }

    /// Reindex the session database if the index is stale.
    pub fn reindex_if_stale(&self, max_age: Duration) -> Result<bool> {
        if !self.should_reindex(max_age) {
            return Ok(false);
        }
        self.reindex_all()?;
        Ok(true)
    }

    fn with_lock<T>(&self, f: impl FnOnce(&SqliteConnection) -> Result<T>) -> Result<T> {
        if let Some(parent) = self.db_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let lock_file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.lock_path)?;
        let _lock = lock_file_guard(&lock_file, Duration::from_secs(5))?;

        let config = SqliteConfig::file(self.db_path.to_string_lossy())
            .flags(OpenFlags::create_read_write())
            .busy_timeout(5000);

        let conn = SqliteConnection::open(&config)
            .map_err(|e| Error::session(format!("SQLite open: {e}")))?;

        // Set pragmas for performance
        conn.execute_raw("PRAGMA journal_mode = WAL")
            .map_err(|e| Error::session(format!("PRAGMA journal_mode: {e}")))?;
        conn.execute_raw("PRAGMA synchronous = NORMAL")
            .map_err(|e| Error::session(format!("PRAGMA synchronous: {e}")))?;
        conn.execute_raw("PRAGMA wal_autocheckpoint = 1000")
            .map_err(|e| Error::session(format!("PRAGMA wal_autocheckpoint: {e}")))?;
        conn.execute_raw("PRAGMA foreign_keys = ON")
            .map_err(|e| Error::session(format!("PRAGMA foreign_keys: {e}")))?;

        f(&conn)
    }

    fn sessions_root(&self) -> &Path {
        self.db_path.parent().unwrap_or_else(|| Path::new("."))
    }
}

impl Default for SessionIndex {
    fn default() -> Self {
        Self::new()
    }
}

fn init_schema(conn: &SqliteConnection) -> Result<()> {
    conn.execute_raw(
        "CREATE TABLE IF NOT EXISTS sessions (
            path TEXT PRIMARY KEY,
            id TEXT NOT NULL,
            cwd TEXT NOT NULL,
            timestamp TEXT NOT NULL,
            message_count INTEGER NOT NULL,
            last_modified_ms INTEGER NOT NULL,
            size_bytes INTEGER NOT NULL,
            name TEXT
        )",
    )
    .map_err(|e| Error::session(format!("Create sessions table: {e}")))?;

    conn.execute_raw(
        "CREATE TABLE IF NOT EXISTS meta (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        )",
    )
    .map_err(|e| Error::session(format!("Create meta table: {e}")))?;

    Ok(())
}

fn row_to_meta(row: &sqlmodel_core::Row) -> Result<SessionMeta> {
    Ok(SessionMeta {
        path: row
            .get_named("path")
            .map_err(|e| Error::session(format!("get path: {e}")))?,
        id: row
            .get_named("id")
            .map_err(|e| Error::session(format!("get id: {e}")))?,
        cwd: row
            .get_named("cwd")
            .map_err(|e| Error::session(format!("get cwd: {e}")))?,
        timestamp: row
            .get_named("timestamp")
            .map_err(|e| Error::session(format!("get timestamp: {e}")))?,
        message_count: u64::try_from(
            row.get_named::<i64>("message_count")
                .map_err(|e| Error::session(format!("get message_count: {e}")))?,
        )
        .unwrap_or(0),
        last_modified_ms: row
            .get_named("last_modified_ms")
            .map_err(|e| Error::session(format!("get last_modified_ms: {e}")))?,
        size_bytes: u64::try_from(
            row.get_named::<i64>("size_bytes")
                .map_err(|e| Error::session(format!("get size_bytes: {e}")))?,
        )
        .unwrap_or(0),
        name: row
            .get_named::<Option<String>>("name")
            .map_err(|e| Error::session(format!("get name: {e}")))?,
    })
}

fn build_meta(
    path: &Path,
    header: &SessionHeader,
    entries: &[SessionEntry],
) -> Result<SessionMeta> {
    let (message_count, name) = session_stats(entries);
    let (last_modified_ms, size_bytes) = file_stats(path)?;
    Ok(SessionMeta {
        path: path.display().to_string(),
        id: header.id.clone(),
        cwd: header.cwd.clone(),
        timestamp: header.timestamp.clone(),
        message_count,
        last_modified_ms,
        size_bytes,
        name,
    })
}

fn build_meta_from_file(path: &Path) -> Result<SessionMeta> {
    let content = fs::read_to_string(path)
        .map_err(|err| Error::session(format!("Read session file {}: {err}", path.display())))?;
    let mut lines = content.lines();
    let header: SessionHeader = lines
        .next()
        .ok_or_else(|| Error::session(format!("Empty session file {}", path.display())))
        .and_then(|line| {
            serde_json::from_str(line).map_err(|err| {
                Error::session(format!("Parse session header {}: {err}", path.display()))
            })
        })?;

    let mut entries = Vec::new();
    for line in lines {
        if let Ok(entry) = serde_json::from_str::<SessionEntry>(line) {
            entries.push(entry);
        }
    }

    build_meta(path, &header, &entries)
}

fn session_stats(entries: &[SessionEntry]) -> (u64, Option<String>) {
    let mut message_count = 0u64;
    let mut name = None;
    for entry in entries {
        match entry {
            SessionEntry::Message(_) => message_count += 1,
            SessionEntry::SessionInfo(info) => {
                if info.name.is_some() {
                    name.clone_from(&info.name);
                }
            }
            _ => {}
        }
    }
    (message_count, name)
}

fn file_stats(path: &Path) -> Result<(i64, u64)> {
    let meta = fs::metadata(path)?;
    let size = meta.len();
    let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    let millis = modified
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let ms = i64::try_from(millis).unwrap_or(i64::MAX);
    Ok((ms, size))
}

fn walk_jsonl(root: &Path) -> Vec<std::io::Result<PathBuf>> {
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir(root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(walk_jsonl(&path));
            } else if path.extension().is_some_and(|ext| ext == "jsonl") {
                out.push(Ok(path));
            }
        }
    }
    out
}

fn current_epoch_ms() -> String {
    chrono::Utc::now().timestamp_millis().to_string()
}

fn lock_file_guard(file: &File, timeout: Duration) -> Result<LockGuard<'_>> {
    let start = Instant::now();
    loop {
        if matches!(FileExt::try_lock_exclusive(file), Ok(true)) {
            return Ok(LockGuard { file });
        }

        if start.elapsed() >= timeout {
            return Err(Error::session(
                "Timed out waiting for session index lock".to_string(),
            ));
        }

        std::thread::sleep(Duration::from_millis(50));
    }
}

struct LockGuard<'a> {
    file: &'a File,
}

impl Drop for LockGuard<'_> {
    fn drop(&mut self) {
        let _ = FileExt::unlock(self.file);
    }
}

#[cfg(test)]
#[path = "../tests/common/mod.rs"]
mod test_common;

#[cfg(test)]
mod tests {
    use super::*;

    use super::test_common::TestHarness;
    use crate::model::UserContent;
    use crate::session::{EntryBase, MessageEntry, SessionInfoEntry, SessionMessage};
    use pretty_assertions::assert_eq;
    use std::fs;
    use std::time::Duration;

    fn write_session_jsonl(path: &Path, header: &SessionHeader, entries: &[SessionEntry]) {
        let mut jsonl = String::new();
        jsonl.push_str(&serde_json::to_string(header).expect("serialize session header"));
        jsonl.push('\n');
        for entry in entries {
            jsonl.push_str(&serde_json::to_string(entry).expect("serialize session entry"));
            jsonl.push('\n');
        }
        fs::write(path, jsonl).expect("write session jsonl");
    }

    fn make_header(id: &str, cwd: &str) -> SessionHeader {
        let mut header = SessionHeader::new();
        header.id = id.to_string();
        header.cwd = cwd.to_string();
        header
    }

    fn make_user_entry(parent_id: Option<String>, id: &str, text: &str) -> SessionEntry {
        SessionEntry::Message(MessageEntry {
            base: EntryBase::new(parent_id, id.to_string()),
            message: SessionMessage::User {
                content: UserContent::Text(text.to_string()),
                timestamp: Some(chrono::Utc::now().timestamp_millis()),
            },
        })
    }

    fn make_session_info_entry(
        parent_id: Option<String>,
        id: &str,
        name: Option<&str>,
    ) -> SessionEntry {
        SessionEntry::SessionInfo(SessionInfoEntry {
            base: EntryBase::new(parent_id, id.to_string()),
            name: name.map(ToString::to_string),
        })
    }

    fn read_meta_last_sync_epoch_ms(index: &SessionIndex) -> String {
        index
            .with_lock(|conn| {
                init_schema(conn)?;
                let rows = conn
                    .query_sync(
                        "SELECT value FROM meta WHERE key='last_sync_epoch_ms' LIMIT 1",
                        &[],
                    )
                    .map_err(|err| Error::session(format!("Query meta failed: {err}")))?;
                let row = rows
                    .into_iter()
                    .next()
                    .ok_or_else(|| Error::session("Missing meta row".to_string()))?;
                row.get_named::<String>("value")
                    .map_err(|err| Error::session(format!("get meta value: {err}")))
            })
            .expect("read meta.last_sync_epoch_ms")
    }

    #[test]
    fn index_session_on_in_memory_session_is_noop() {
        let harness = TestHarness::new("index_session_on_in_memory_session_is_noop");
        let root = harness.temp_path("sessions");
        fs::create_dir_all(&root).expect("create root dir");
        let index = SessionIndex::for_sessions_root(&root);
        let session = Session::in_memory();

        index
            .index_session(&session)
            .expect("index in-memory session");

        harness
            .log()
            .info_ctx("verify", "No index files created", |ctx| {
                ctx.push(("db_path".into(), index.db_path.display().to_string()));
                ctx.push(("lock_path".into(), index.lock_path.display().to_string()));
            });
        assert!(!index.db_path.exists());
        assert!(!index.lock_path.exists());
    }

    #[test]
    fn index_session_inserts_row_and_updates_meta() {
        let harness = TestHarness::new("index_session_inserts_row_and_updates_meta");
        let root = harness.temp_path("sessions");
        fs::create_dir_all(&root).expect("create root dir");
        let index = SessionIndex::for_sessions_root(&root);

        let session_path = harness.temp_path("sessions/project/a.jsonl");
        fs::create_dir_all(session_path.parent().expect("parent")).expect("create session dir");
        fs::write(&session_path, "hello").expect("write session file");

        let mut session = Session::in_memory();
        session.header = make_header("id-a", "cwd-a");
        session.path = Some(session_path.clone());
        session.entries.push(make_user_entry(None, "m1", "hi"));

        index.index_session(&session).expect("index session");

        let sessions = index.list_sessions(Some("cwd-a")).expect("list sessions");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "id-a");
        assert_eq!(sessions[0].cwd, "cwd-a");
        assert_eq!(sessions[0].message_count, 1);
        assert_eq!(sessions[0].path, session_path.display().to_string());

        let meta_value = read_meta_last_sync_epoch_ms(&index);
        harness
            .log()
            .info_ctx("verify", "meta.last_sync_epoch_ms present", |ctx| {
                ctx.push(("value".into(), meta_value.clone()));
            });
        assert!(
            meta_value.parse::<i64>().is_ok(),
            "Expected meta value to be an integer epoch ms"
        );
    }

    #[test]
    fn index_session_updates_existing_row() {
        let harness = TestHarness::new("index_session_updates_existing_row");
        let root = harness.temp_path("sessions");
        fs::create_dir_all(&root).expect("create root dir");
        let index = SessionIndex::for_sessions_root(&root);

        let session_path = harness.temp_path("sessions/project/update.jsonl");
        fs::create_dir_all(session_path.parent().expect("parent")).expect("create session dir");
        fs::write(&session_path, "first").expect("write session file");

        let mut session = Session::in_memory();
        session.header = make_header("id-update", "cwd-update");
        session.path = Some(session_path.clone());
        session.entries.push(make_user_entry(None, "m1", "hi"));

        index
            .index_session(&session)
            .expect("index session first time");
        let first_meta = index
            .list_sessions(Some("cwd-update"))
            .expect("list sessions")[0]
            .clone();
        let first_sync = read_meta_last_sync_epoch_ms(&index);

        std::thread::sleep(Duration::from_millis(10));
        fs::write(&session_path, "second-longer").expect("rewrite session file");
        session
            .entries
            .push(make_user_entry(Some("m1".to_string()), "m2", "again"));

        index
            .index_session(&session)
            .expect("index session second time");
        let second_meta = index
            .list_sessions(Some("cwd-update"))
            .expect("list sessions")[0]
            .clone();
        let second_sync = read_meta_last_sync_epoch_ms(&index);

        harness.log().info_ctx("verify", "row updated", |ctx| {
            ctx.push((
                "first_message_count".into(),
                first_meta.message_count.to_string(),
            ));
            ctx.push((
                "second_message_count".into(),
                second_meta.message_count.to_string(),
            ));
            ctx.push(("first_size".into(), first_meta.size_bytes.to_string()));
            ctx.push(("second_size".into(), second_meta.size_bytes.to_string()));
            ctx.push(("first_sync".into(), first_sync.clone()));
            ctx.push(("second_sync".into(), second_sync.clone()));
        });

        assert_eq!(second_meta.message_count, 2);
        assert!(second_meta.size_bytes >= first_meta.size_bytes);
        assert!(second_meta.last_modified_ms >= first_meta.last_modified_ms);
        assert!(second_sync.parse::<i64>().unwrap_or(0) >= first_sync.parse::<i64>().unwrap_or(0));
    }

    #[test]
    fn list_sessions_orders_by_last_modified_desc() {
        let harness = TestHarness::new("list_sessions_orders_by_last_modified_desc");
        let root = harness.temp_path("sessions");
        fs::create_dir_all(&root).expect("create root dir");
        let index = SessionIndex::for_sessions_root(&root);

        let path_a = harness.temp_path("sessions/project/a.jsonl");
        fs::create_dir_all(path_a.parent().expect("parent")).expect("create dirs");
        fs::write(&path_a, "a").expect("write file a");

        let mut session_a = Session::in_memory();
        session_a.header = make_header("id-a", "cwd-a");
        session_a.path = Some(path_a);
        session_a.entries.push(make_user_entry(None, "m1", "a"));
        index.index_session(&session_a).expect("index a");

        std::thread::sleep(Duration::from_millis(10));

        let path_b = harness.temp_path("sessions/project/b.jsonl");
        fs::create_dir_all(path_b.parent().expect("parent")).expect("create dirs");
        fs::write(&path_b, "bbbbb").expect("write file b");

        let mut session_b = Session::in_memory();
        session_b.header = make_header("id-b", "cwd-b");
        session_b.path = Some(path_b);
        session_b.entries.push(make_user_entry(None, "m1", "b"));
        index.index_session(&session_b).expect("index b");

        let sessions = index.list_sessions(None).expect("list sessions");
        harness
            .log()
            .info("verify", format!("listed {} sessions", sessions.len()));
        assert!(sessions.len() >= 2);
        assert_eq!(sessions[0].id, "id-b");
        assert_eq!(sessions[1].id, "id-a");
        assert!(sessions[0].last_modified_ms >= sessions[1].last_modified_ms);
    }

    #[test]
    fn list_sessions_filters_by_cwd() {
        let harness = TestHarness::new("list_sessions_filters_by_cwd");
        let root = harness.temp_path("sessions");
        fs::create_dir_all(&root).expect("create root dir");
        let index = SessionIndex::for_sessions_root(&root);

        for (id, cwd) in [("id-a", "cwd-a"), ("id-b", "cwd-b")] {
            let path = harness.temp_path(format!("sessions/project/{id}.jsonl"));
            fs::create_dir_all(path.parent().expect("parent")).expect("create dirs");
            fs::write(&path, id).expect("write session file");

            let mut session = Session::in_memory();
            session.header = make_header(id, cwd);
            session.path = Some(path);
            session.entries.push(make_user_entry(None, "m1", id));
            index.index_session(&session).expect("index session");
        }

        let only_a = index
            .list_sessions(Some("cwd-a"))
            .expect("list sessions cwd-a");
        assert_eq!(only_a.len(), 1);
        assert_eq!(only_a[0].id, "id-a");
    }

    #[test]
    fn reindex_all_is_noop_when_sessions_root_missing() {
        let harness = TestHarness::new("reindex_all_is_noop_when_sessions_root_missing");
        let missing_root = harness.temp_path("does-not-exist");
        let index = SessionIndex::for_sessions_root(&missing_root);

        index.reindex_all().expect("reindex_all");
        assert!(!index.db_path.exists());
        assert!(!index.lock_path.exists());
    }

    #[test]
    fn reindex_all_rebuilds_index_from_disk() {
        let harness = TestHarness::new("reindex_all_rebuilds_index_from_disk");
        let root = harness.temp_path("sessions");
        fs::create_dir_all(&root).expect("create root dir");
        let index = SessionIndex::for_sessions_root(&root);

        let path = harness.temp_path("sessions/project/reindex.jsonl");
        fs::create_dir_all(path.parent().expect("parent")).expect("create dirs");

        let header = make_header("id-reindex", "cwd-reindex");
        let entries = vec![
            make_user_entry(None, "m1", "hello"),
            make_session_info_entry(Some("m1".to_string()), "info1", Some("My Session")),
            make_user_entry(Some("info1".to_string()), "m2", "world"),
        ];
        write_session_jsonl(&path, &header, &entries);

        index.reindex_all().expect("reindex_all");

        let sessions = index
            .list_sessions(Some("cwd-reindex"))
            .expect("list sessions");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "id-reindex");
        assert_eq!(sessions[0].message_count, 2);
        assert_eq!(sessions[0].name.as_deref(), Some("My Session"));

        let meta_value = read_meta_last_sync_epoch_ms(&index);
        harness.log().info_ctx("verify", "meta updated", |ctx| {
            ctx.push(("value".into(), meta_value.clone()));
        });
        assert!(meta_value.parse::<i64>().unwrap_or(0) > 0);
    }

    #[test]
    fn reindex_all_skips_invalid_jsonl_files() {
        let harness = TestHarness::new("reindex_all_skips_invalid_jsonl_files");
        let root = harness.temp_path("sessions");
        fs::create_dir_all(&root).expect("create root dir");
        let index = SessionIndex::for_sessions_root(&root);

        let good = harness.temp_path("sessions/project/good.jsonl");
        fs::create_dir_all(good.parent().expect("parent")).expect("create dirs");
        let header = make_header("id-good", "cwd-good");
        let entries = vec![make_user_entry(None, "m1", "ok")];
        write_session_jsonl(&good, &header, &entries);

        let bad = harness.temp_path("sessions/project/bad.jsonl");
        fs::write(&bad, "not-json\n{").expect("write bad jsonl");

        index.reindex_all().expect("reindex_all should succeed");
        let sessions = index.list_sessions(None).expect("list sessions");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "id-good");
    }

    #[test]
    fn build_meta_from_file_returns_session_error_on_invalid_header() {
        let harness =
            TestHarness::new("build_meta_from_file_returns_session_error_on_invalid_header");
        let path = harness.temp_path("bad_header.jsonl");
        fs::write(&path, "not json\n").expect("write bad header");

        let err = build_meta_from_file(&path).expect_err("expected error");
        harness.log().info("verify", format!("error: {err}"));

        assert!(
            matches!(err, Error::Session(ref msg) if msg.contains("Parse session header")),
            "Expected Error::Session containing Parse session header, got {err:?}",
        );
    }

    #[test]
    fn build_meta_from_file_returns_session_error_on_empty_file() {
        let harness = TestHarness::new("build_meta_from_file_returns_session_error_on_empty_file");
        let path = harness.temp_path("empty.jsonl");
        fs::write(&path, "").expect("write empty");

        let err = build_meta_from_file(&path).expect_err("expected error");
        if let Error::Session(msg) = &err {
            harness.log().info("verify", msg.clone());
        }
        assert!(
            matches!(err, Error::Session(ref msg) if msg.contains("Empty session file")),
            "Expected Error::Session containing Empty session file, got {err:?}",
        );
    }

    #[test]
    fn list_sessions_returns_session_error_when_db_path_is_directory() {
        let harness =
            TestHarness::new("list_sessions_returns_session_error_when_db_path_is_directory");
        let root = harness.temp_path("sessions");
        fs::create_dir_all(&root).expect("create root dir");

        let db_dir = root.join("session-index.sqlite");
        fs::create_dir_all(&db_dir).expect("create db dir to force sqlite open failure");

        let index = SessionIndex::for_sessions_root(&root);
        let err = index.list_sessions(None).expect_err("expected error");
        if let Error::Session(msg) = &err {
            harness.log().info("verify", msg.clone());
        }
        assert!(
            matches!(err, Error::Session(ref msg) if msg.contains("SQLite open")),
            "Expected Error::Session containing SQLite open, got {err:?}",
        );
    }

    #[test]
    fn lock_file_guard_prevents_concurrent_access() {
        let harness = TestHarness::new("lock_file_guard_prevents_concurrent_access");
        let path = harness.temp_path("lockfile.lock");
        fs::write(&path, "").expect("create lock file");

        let file1 = File::options()
            .read(true)
            .write(true)
            .open(&path)
            .expect("open file1");
        let file2 = File::options()
            .read(true)
            .write(true)
            .open(&path)
            .expect("open file2");

        let guard1 = lock_file_guard(&file1, Duration::from_millis(50)).expect("acquire lock");
        let err = lock_file_guard(&file2, Duration::from_millis(50))
            .err()
            .expect("expected lock timeout");
        drop(guard1);

        assert!(
            matches!(err, Error::Session(ref msg) if msg.contains("Timed out")),
            "Expected Error::Session containing Timed out, got {err:?}",
        );

        let _guard2 =
            lock_file_guard(&file2, Duration::from_millis(50)).expect("lock after release");
    }

    #[test]
    fn should_reindex_returns_true_when_db_missing() {
        let harness = TestHarness::new("should_reindex_returns_true_when_db_missing");
        let root = harness.temp_path("sessions");
        fs::create_dir_all(&root).expect("create root dir");
        let index = SessionIndex::for_sessions_root(&root);

        assert!(index.should_reindex(Duration::from_secs(60)));
    }
}
