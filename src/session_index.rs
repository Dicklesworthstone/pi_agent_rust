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
        let db_path = Config::global_dir().join("session-index.sqlite");
        let lock_path = Config::global_dir().join("session-index.lock");
        Self { db_path, lock_path }
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

    pub fn reindex_all(&self) -> Result<()> {
        let sessions_root = Config::sessions_dir();
        if !sessions_root.exists() {
            return Ok(());
        }

        let mut metas = Vec::new();
        for entry in walk_jsonl(&sessions_root) {
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
    let content = fs::read_to_string(path)?;
    let mut lines = content.lines();
    let header: SessionHeader = lines
        .next()
        .map(serde_json::from_str)
        .transpose()?
        .ok_or_else(|| Error::session("Empty session file"))?;

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
