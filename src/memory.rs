//! Project-scoped memory bank (bd-cv653.4.1).
//!
//! The agent remembers the codebase between sessions: `retain` writes
//! durable facts/lessons, `recall` searches them (FTS5 + recency/frequency
//! ranking), `reflect` synthesizes an answer over the bank with citation
//! ids, and `memory_edit` updates/invalidates/forgets by id. Project-scoped
//! by default: the bank for this repo stays with this repo.
//!
//! Store: per-project SQLite under the agent config dir
//! (`<global_dir>/memory/<project-key>.sqlite`), project-key = SHA-256 of
//! the canonicalized primary root path — the config dir is the stable
//! anchor (session dirs are overridable). WAL + the session-index locking
//! discipline. FTS5 via the `fsqlite/fts5` feature (spike: tests/fts5_spike.rs).
//!
//! Ranking is behind a small trait so a later hybrid semantic+keyword
//! engine (frankensearch-style) can replace FTS scoring without tool-schema
//! changes.
//!
//! Privacy: `retain` screens content through a dedicated secret screener.
//! bd-cv653.7.9 (pattern vault + entropy rules) has not landed yet, so the
//! in-module screener is the interim floor; TODO(.7.9): swap to the shared
//! vault when it lands so one detector serves every surface.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;

use crate::error::{Error, Result};
use crate::session_sqlite::{SqliteConnection, run_on_sqlite_thread};

/// Tool-result schema tag for memory operations (stable audit contract).
pub const MEMORY_SCHEMA: &str = "pi.memory.v1";

/// Default recall result cap.
const DEFAULT_RECALL_LIMIT: usize = 10;

/// Mental-model startup block budget (bytes of memory content).
const MENTAL_MODEL_BUDGET: usize = 4 * 1024;

/// Memory kinds (omp parity).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum MemoryKind {
    Fact,
    Lesson,
    Preference,
    Decision,
}

impl MemoryKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fact => "fact",
            Self::Lesson => "lesson",
            Self::Preference => "preference",
            Self::Decision => "decision",
        }
    }

    fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "fact" => Ok(Self::Fact),
            "lesson" => Ok(Self::Lesson),
            "preference" => Ok(Self::Preference),
            "decision" => Ok(Self::Decision),
            other => Err(Error::validation(format!(
                "Unknown memory kind '{other}'; expected fact, lesson, preference, or decision"
            ))),
        }
    }
}

/// One stored memory.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Memory {
    pub schema: String,
    pub id: i64,
    pub kind: String,
    pub content: String,
    pub tags: Vec<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub session_id: Option<String>,
    pub status: String,
    pub supersedes: Option<i64>,
}

/// Memory edit operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryEditOp {
    /// Replace content (bumps updated_at; audit-logged).
    Update,
    /// Tombstone: excluded from recall/mental-model but auditable.
    Invalidate,
    /// Hard delete the row.
    Forget,
}

impl MemoryEditOp {
    fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "update" => Ok(Self::Update),
            "invalidate" => Ok(Self::Invalidate),
            "forget" => Ok(Self::Forget),
            other => Err(Error::validation(format!(
                "Unknown memory_edit op '{other}'; expected update, invalidate, or forget"
            ))),
        }
    }
}

/// Ranking surface: v1 is FTS5 + recency; a later hybrid engine implements
/// this trait without touching tool schemas.
trait MemoryRanker {
    fn recall(&self, conn: &SqliteConnection, query: &str, limit: usize) -> Result<Vec<Memory>>;
}

/// FTS5 MATCH + recency ordering.
struct FtsRecencyRanker;

impl MemoryRanker for FtsRecencyRanker {
    fn recall(&self, conn: &SqliteConnection, query: &str, limit: usize) -> Result<Vec<Memory>> {
        // Sanitize the FTS query: quote each whitespace-separated token so
        // user text can't break MATCH syntax (AND semantics across tokens).
        let fts_query = query
            .split_whitespace()
            .filter(|token| !token.is_empty())
            .map(|token| format!("\"{}\"", token.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" ");
        if fts_query.is_empty() {
            return Ok(Vec::new());
        }
        let rows = conn
            .query_sync(
                "SELECT m.id, m.kind, m.content, m.tags, m.created_at_ms, m.updated_at_ms, \
                        m.session_id, m.status, m.supersedes \
                 FROM memories_fts f \
                 JOIN memories m ON m.id = f.rowid \
                 WHERE memories_fts MATCH ?1 AND m.status = 'active' \
                 ORDER BY m.updated_at_ms DESC \
                 LIMIT ?2",
                &[
                    fsqlite::SqliteValue::Text(fts_query.into()),
                    fsqlite::SqliteValue::Integer(i64::try_from(limit).unwrap_or(10)),
                ],
            )
            .map_err(|e| Error::tool("memory", format!("recall query failed: {e}")))?;
        rows.iter().map(row_to_memory).collect()
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn row_text(row: &fsqlite::Row, index: usize) -> Result<String> {
    row.values()
        .get(index)
        .map(|value| match value {
            fsqlite::SqliteValue::Text(text) => text.to_string(),
            fsqlite::SqliteValue::Integer(number) => number.to_string(),
            fsqlite::SqliteValue::Null => String::new(),
            other => format!("{other:?}"),
        })
        .ok_or_else(|| Error::tool("memory", format!("missing column {index}")))
}

fn row_i64(row: &fsqlite::Row, index: usize) -> Result<i64> {
    row.values()
        .get(index)
        .and_then(|value| match value {
            fsqlite::SqliteValue::Integer(number) => Some(*number),
            _ => None,
        })
        .ok_or_else(|| Error::tool("memory", format!("missing integer column {index}")))
}

fn row_opt_text(row: &fsqlite::Row, index: usize) -> Option<String> {
    row.values().get(index).and_then(|value| match value {
        fsqlite::SqliteValue::Text(text) => Some(text.to_string()),
        _ => None,
    })
}

fn row_to_memory(row: &fsqlite::Row) -> Result<Memory> {
    let tags_raw = row_text(row, 3)?;
    let tags: Vec<String> = serde_json::from_str(&tags_raw).unwrap_or_default();
    Ok(Memory {
        schema: MEMORY_SCHEMA.to_string(),
        id: row_i64(row, 0)?,
        kind: row_text(row, 1)?,
        content: row_text(row, 2)?,
        tags,
        created_at_ms: row_i64(row, 4)?,
        updated_at_ms: row_i64(row, 5)?,
        session_id: row_opt_text(row, 6),
        status: row_text(row, 7)?,
        supersedes: row.values().get(8).and_then(|value| match value {
            fsqlite::SqliteValue::Integer(number) => Some(*number),
            _ => None,
        }),
    })
}

/// Per-project memory store (SQLite + FTS5).
pub struct MemoryStore {
    db_path: PathBuf,
    project_key: String,
}

impl MemoryStore {
    /// Open (creating) the store for a project root.
    ///
    /// # Errors
    /// IO errors creating the memory dir.
    pub fn open(project_root: &Path) -> Result<Self> {
        let canonical = project_root
            .canonicalize()
            .unwrap_or_else(|_| project_root.to_path_buf());
        let key = crate::package_manager::hex_encode(&sha2::Sha256::digest(
            canonical.to_string_lossy().as_bytes(),
        ));
        let short_key: String = key.chars().take(32).collect();
        let dir = crate::config::Config::global_dir().join("memory");
        std::fs::create_dir_all(&dir)
            .map_err(|e| Error::tool("memory", format!("Failed to create memory dir: {e}")))?;
        Ok(Self {
            db_path: dir.join(format!("{short_key}.sqlite")),
            project_key: short_key,
        })
    }

    #[must_use]
    pub fn project_key(&self) -> &str {
        &self.project_key
    }

    fn with_conn<T: Send>(
        &self,
        f: impl FnOnce(&SqliteConnection) -> Result<T> + Send,
    ) -> Result<T> {
        let db_path = self.db_path.clone();
        run_on_sqlite_thread(move || {
            let conn = SqliteConnection::open_read_write(&db_path)
                .map_err(|e| Error::tool("memory", format!("SQLite open: {e}")))?;
            conn.execute_raw(
                "PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = NORMAL;
                 PRAGMA foreign_keys = ON;
                 CREATE TABLE IF NOT EXISTS memories (
                   id INTEGER PRIMARY KEY AUTOINCREMENT,
                   kind TEXT NOT NULL,
                   content TEXT NOT NULL,
                   tags TEXT NOT NULL DEFAULT '[]',
                   created_at_ms INTEGER NOT NULL,
                   updated_at_ms INTEGER NOT NULL,
                   session_id TEXT,
                   status TEXT NOT NULL DEFAULT 'active',
                   supersedes INTEGER
                 );
                 CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts
                   USING fts5(content);
                 CREATE TABLE IF NOT EXISTS memory_audit (
                   id INTEGER PRIMARY KEY AUTOINCREMENT,
                   memory_id INTEGER NOT NULL,
                   op TEXT NOT NULL,
                   at_ms INTEGER NOT NULL
                 );",
            )
            .map_err(|e| Error::tool("memory", format!("schema init failed: {e}")))?;
            let result = f(&conn)?;
            conn.close()
                .map_err(|e| Error::tool("memory", format!("SQLite close: {e}")))?;
            Ok(result)
        })
    }

    /// Insert a fact/lesson/preference/decision after dedupe + secret
    /// screening. Returns the stored row (content may be redacted).
    ///
    /// # Errors
    /// Store errors; named `PI_MEMORY_DUPLICATE` for exact active dupes.
    pub fn retain(
        &self,
        kind: MemoryKind,
        content: &str,
        tags: &[String],
        session_id: Option<&str>,
    ) -> Result<Memory> {
        let content = screen_secrets(content);
        let now = now_ms();
        let tags_json = serde_json::to_string(tags).unwrap_or_else(|_| "[]".to_string());
        let session_id = session_id.map(str::to_string);
        let kind_str = kind.as_str().to_string();
        self.with_conn(move |conn| {
            let dupes = conn
                .query_sync(
                    "SELECT COUNT(*) FROM memories WHERE content = ?1 AND status = 'active'",
                    &[fsqlite::SqliteValue::Text(content.clone().into())],
                )
                .map_err(|e| Error::tool("memory", format!("dedupe check failed: {e}")))?;
            let dupe_count = dupes.first().map_or(Ok(0), |row| row_i64(row, 0))?;
            if dupe_count > 0 {
                return Err(Error::tool(
                    "memory",
                    "PI_MEMORY_DUPLICATE: an identical active memory already exists".to_string(),
                ));
            }
            conn.execute_sync(
                "INSERT INTO memories (kind, content, tags, created_at_ms, updated_at_ms, \
                 session_id, status) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active')",
                &[
                    fsqlite::SqliteValue::Text(kind_str.into()),
                    fsqlite::SqliteValue::Text(content.clone().into()),
                    fsqlite::SqliteValue::Text(tags_json.into()),
                    fsqlite::SqliteValue::Integer(now),
                    fsqlite::SqliteValue::Integer(now),
                    session_id.clone().map_or(fsqlite::SqliteValue::Null, |id| {
                        fsqlite::SqliteValue::Text(id.into())
                    }),
                ],
            )
            .map_err(|e| Error::tool("memory", format!("retain insert failed: {e}")))?;
            let id_rows = conn
                .query_sync("SELECT last_insert_rowid()", &[])
                .map_err(|e| Error::tool("memory", format!("rowid lookup failed: {e}")))?;
            let id = id_rows.first().map_or(Ok(0), |row| row_i64(row, 0))?;
            conn.execute_sync(
                "INSERT INTO memories_fts (rowid, content) VALUES (?1, ?2)",
                &[
                    fsqlite::SqliteValue::Integer(id),
                    fsqlite::SqliteValue::Text(content.clone().into()),
                ],
            )
            .map_err(|e| Error::tool("memory", format!("fts index failed: {e}")))?;
            Ok(Memory {
                schema: MEMORY_SCHEMA.to_string(),
                id,
                kind: kind.as_str().to_string(),
                content,
                tags: tags.to_vec(),
                created_at_ms: now,
                updated_at_ms: now,
                session_id,
                status: "active".to_string(),
                supersedes: None,
            })
        })
    }

    /// FTS-ranked active memories matching the query.
    ///
    /// # Errors
    /// Query errors.
    pub fn recall(&self, query: &str, limit: Option<usize>) -> Result<Vec<Memory>> {
        let limit = limit.unwrap_or(DEFAULT_RECALL_LIMIT).min(100);
        let query = query.to_string();
        self.with_conn(move |conn| FtsRecencyRanker.recall(conn, &query, limit))
    }

    /// Apply an edit op (audit-logged).
    ///
    /// # Errors
    /// Named `PI_MEMORY_UNKNOWN_ID` for unknown ids.
    pub fn edit(&self, id: i64, op: MemoryEditOp, content: Option<&str>) -> Result<()> {
        let now = now_ms();
        let new_content = content.map(screen_secrets);
        self.with_conn(move |conn| {
            let exists = conn
                .query_sync(
                    "SELECT COUNT(*) FROM memories WHERE id = ?1",
                    &[fsqlite::SqliteValue::Integer(id)],
                )
                .map_err(|e| Error::tool("memory", format!("lookup failed: {e}")))?;
            let count = exists.first().map_or(Ok(0), |row| row_i64(row, 0))?;
            if count == 0 {
                return Err(Error::tool(
                    "memory",
                    format!("PI_MEMORY_UNKNOWN_ID: no memory with id {id}"),
                ));
            }
            match op {
                MemoryEditOp::Update => {
                    let Some(new_content) = new_content.clone() else {
                        return Err(Error::validation(
                            "memory_edit update requires content".to_string(),
                        ));
                    };
                    conn.execute_sync(
                        "UPDATE memories SET content = ?1, updated_at_ms = ?2 WHERE id = ?3",
                        &[
                            fsqlite::SqliteValue::Text(new_content.clone().into()),
                            fsqlite::SqliteValue::Integer(now),
                            fsqlite::SqliteValue::Integer(id),
                        ],
                    )
                    .map_err(|e| Error::tool("memory", format!("update failed: {e}")))?;
                    conn.execute_sync(
                        "UPDATE memories_fts SET content = ?1 WHERE rowid = ?2",
                        &[
                            fsqlite::SqliteValue::Text(new_content.into()),
                            fsqlite::SqliteValue::Integer(id),
                        ],
                    )
                    .map_err(|e| Error::tool("memory", format!("fts update failed: {e}")))?;
                }
                MemoryEditOp::Invalidate => {
                    conn.execute_sync(
                        "UPDATE memories SET status = 'invalidated', updated_at_ms = ?1 \
                         WHERE id = ?2",
                        &[
                            fsqlite::SqliteValue::Integer(now),
                            fsqlite::SqliteValue::Integer(id),
                        ],
                    )
                    .map_err(|e| Error::tool("memory", format!("invalidate failed: {e}")))?;
                }
                MemoryEditOp::Forget => {
                    conn.execute_sync(
                        "DELETE FROM memories WHERE id = ?1",
                        &[fsqlite::SqliteValue::Integer(id)],
                    )
                    .map_err(|e| Error::tool("memory", format!("forget failed: {e}")))?;
                    conn.execute_sync(
                        "DELETE FROM memories_fts WHERE rowid = ?1",
                        &[fsqlite::SqliteValue::Integer(id)],
                    )
                    .map_err(|e| Error::tool("memory", format!("fts forget failed: {e}")))?;
                }
            }
            conn.execute_sync(
                "INSERT INTO memory_audit (memory_id, op, at_ms) VALUES (?1, ?2, ?3)",
                &[
                    fsqlite::SqliteValue::Integer(id),
                    fsqlite::SqliteValue::Text(
                        match op {
                            MemoryEditOp::Update => "update",
                            MemoryEditOp::Invalidate => "invalidate",
                            MemoryEditOp::Forget => "forget",
                        }
                        .into(),
                    ),
                    fsqlite::SqliteValue::Integer(now),
                ],
            )
            .map_err(|e| Error::tool("memory", format!("audit write failed: {e}")))?;
            Ok(())
        })
    }

    /// Budget-capped mental-model block for the system prompt: top active
    /// facts/lessons, newest first, capped at [`MENTAL_MODEL_BUDGET`].
    ///
    /// # Errors
    /// Query errors.
    pub fn mental_model(&self) -> Result<String> {
        self.with_conn(move |conn| {
            let rows = conn
                .query_sync(
                    "SELECT id, kind, content, tags, created_at_ms, updated_at_ms, session_id, \
                            status, supersedes \
                     FROM memories WHERE status = 'active' \
                     ORDER BY updated_at_ms DESC LIMIT 50",
                    &[],
                )
                .map_err(|e| Error::tool("memory", format!("mental model query failed: {e}")))?;
            let mut block = String::from("<memory>\n");
            let mut used = block.len();
            for row in &rows {
                let memory = row_to_memory(row)?;
                let line = format!("- [{}] ({}): {}\n", memory.id, memory.kind, memory.content);
                if used + line.len() > MENTAL_MODEL_BUDGET {
                    break;
                }
                block.push_str(&line);
                used += line.len();
            }
            block.push_str("</memory>");
            Ok(if rows.is_empty() {
                String::new()
            } else {
                block
            })
        })
    }

    /// List memories (newest first) for the /memory surface.
    ///
    /// # Errors
    /// Query errors.
    pub fn list(&self, limit: usize) -> Result<Vec<Memory>> {
        self.with_conn(move |conn| {
            let rows = conn
                .query_sync(
                    "SELECT id, kind, content, tags, created_at_ms, updated_at_ms, session_id, \
                            status, supersedes \
                     FROM memories ORDER BY updated_at_ms DESC LIMIT ?1",
                    &[fsqlite::SqliteValue::Integer(
                        i64::try_from(limit).unwrap_or(50),
                    )],
                )
                .map_err(|e| Error::tool("memory", format!("list query failed: {e}")))?;
            rows.iter().map(row_to_memory).collect()
        })
    }
}

// ---------------------------------------------------------------------------
// Secret screening (interim floor until bd-cv653.7.9's vault lands)
// ---------------------------------------------------------------------------

/// Well-known credential shapes screened out of retained content. Each
/// entry: (regex, placeholder). TODO(.7.9): replace with the shared vault.
const SECRET_PATTERNS: &[(&str, &str)] = &[
    (r"sk-ant-[A-Za-z0-9_\-]{16,}", "[REDACTED_ANTHROPIC_KEY]"),
    (r"sk-[A-Za-z0-9_\-]{16,}", "[REDACTED_OPENAI_KEY]"),
    (r"ghp_[A-Za-z0-9]{20,}", "[REDACTED_GITHUB_PAT]"),
    (r"github_pat_[A-Za-z0-9_]{20,}", "[REDACTED_GITHUB_PAT]"),
    (r"AKIA[0-9A-Z]{16}", "[REDACTED_AWS_ACCESS_KEY]"),
    (
        r"-----BEGIN [A-Z ]*PRIVATE KEY-----",
        "[REDACTED_PRIVATE_KEY]",
    ),
    (r"AIza[0-9A-Za-z_\-]{20,}", "[REDACTED_GOOGLE_API_KEY]"),
    (r"xox[baprs]-[A-Za-z0-9\-]{10,}", "[REDACTED_SLACK_TOKEN]"),
];

fn secret_patterns() -> &'static Vec<(regex::Regex, &'static str)> {
    static PATTERNS: std::sync::LazyLock<Vec<(regex::Regex, &'static str)>> =
        std::sync::LazyLock::new(|| {
            SECRET_PATTERNS
                .iter()
                .map(|(pattern, placeholder)| {
                    (
                        regex::Regex::new(pattern).expect("secret pattern compiles"),
                        *placeholder,
                    )
                })
                .collect()
        });
    &PATTERNS
}

/// Replace any detected credential in `content` with a placeholder.
/// Memories never store detected secrets.
#[must_use]
pub fn screen_secrets(content: &str) -> String {
    let mut screened = content.to_string();
    for (pattern, placeholder) in secret_patterns() {
        screened = pattern.replace_all(&screened, *placeholder).into_owned();
    }
    screened
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

use futures::StreamExt as _;

use crate::model::{ContentBlock, TextContent};
use crate::provider::{Context, StreamEvent, StreamOptions};
use crate::tools::{Tool, ToolEffects, ToolOutput, ToolUpdate};

fn text_output(text: String, details: serde_json::Value, is_error: bool) -> ToolOutput {
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(text))],
        details: Some(details),
        is_error,
    }
}

/// `retain`: queue a durable fact/lesson/preference/decision.
pub struct RetainTool {
    store: Arc<MemoryStore>,
}

impl RetainTool {
    #[must_use]
    pub const fn new(store: Arc<MemoryStore>) -> Self {
        Self { store }
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RetainInput {
    content: String,
    kind: Option<String>,
    tags: Option<Vec<String>>,
}

#[async_trait::async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Tool for RetainTool {
    fn name(&self) -> &str {
        "retain"
    }

    fn label(&self) -> &str {
        "retain"
    }

    fn description(&self) -> &str {
        "Queue a durable memory for this project (fact, lesson, preference, \
         or decision) that survives across sessions. Secret-looking content \
         is redacted before storage."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "content": { "type": "string", "description": "The fact or lesson to remember" },
                "kind": {
                    "type": "string",
                    "enum": ["fact", "lesson", "preference", "decision"],
                    "description": "Memory kind (default: fact)"
                },
                "tags": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional tags for later filtering"
                }
            },
            "required": ["content"]
        })
    }

    fn effects(&self) -> ToolEffects {
        ToolEffects::write()
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let input: RetainInput =
            serde_json::from_value(input).map_err(|e| Error::validation(e.to_string()))?;
        if input.content.trim().is_empty() {
            return Err(Error::validation(
                "retain requires non-empty content".to_string(),
            ));
        }
        let kind = input
            .kind
            .as_deref()
            .map_or(Ok(MemoryKind::Fact), MemoryKind::parse)?;
        let tags = input.tags.unwrap_or_default();
        match self.store.retain(kind, &input.content, &tags, None) {
            Ok(memory) => {
                let details = serde_json::to_value(&memory)?;
                let redaction_note = if memory.content.as_str() == input.content {
                    ""
                } else {
                    " (secret redacted before storage)"
                };
                Ok(text_output(
                    format!("Remembered [{}] {}{redaction_note}", memory.id, memory.kind),
                    details,
                    false,
                ))
            }
            Err(err) => Ok(text_output(
                err.to_string(),
                serde_json::json!({ "error": err.to_string() }),
                true,
            )),
        }
    }
}

/// `recall`: search raw memories.
pub struct RecallTool {
    store: Arc<MemoryStore>,
}

impl RecallTool {
    #[must_use]
    pub const fn new(store: Arc<MemoryStore>) -> Self {
        Self { store }
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RecallInput {
    query: String,
    limit: Option<usize>,
}

#[async_trait::async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Tool for RecallTool {
    fn name(&self) -> &str {
        "recall"
    }

    fn label(&self) -> &str {
        "recall"
    }

    fn description(&self) -> &str {
        "Search this project's memories (full-text, ranked by recency). \
         Returns matching memory ids, kinds, and content."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search text (FTS, AND across words)" },
                "limit": { "type": "integer", "description": "Max results (default 10, max 100)" }
            },
            "required": ["query"]
        })
    }

    fn effects(&self) -> ToolEffects {
        ToolEffects::read()
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let input: RecallInput =
            serde_json::from_value(input).map_err(|e| Error::validation(e.to_string()))?;
        let memories = self.store.recall(&input.query, input.limit)?;
        let details = serde_json::json!({
            "schema": MEMORY_SCHEMA,
            "query": input.query,
            "memories": memories,
        });
        let text = if memories.is_empty() {
            format!("No memories match '{}'.", input.query)
        } else {
            memories
                .iter()
                .map(|memory| format!("[{}] ({}): {}", memory.id, memory.kind, memory.content))
                .collect::<Vec<_>>()
                .join("\n")
        };
        Ok(text_output(text, details, false))
    }
}

/// `memory_edit`: update / invalidate / forget by id.
pub struct MemoryEditTool {
    store: Arc<MemoryStore>,
}

impl MemoryEditTool {
    #[must_use]
    pub const fn new(store: Arc<MemoryStore>) -> Self {
        Self { store }
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct MemoryEditInput {
    id: i64,
    op: String,
    content: Option<String>,
}

#[async_trait::async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Tool for MemoryEditTool {
    fn name(&self) -> &str {
        "memory_edit"
    }

    fn label(&self) -> &str {
        "memory edit"
    }

    fn description(&self) -> &str {
        "Edit a memory by id: `update` (replace content), `invalidate` \
         (tombstone — excluded from recall but auditable), or `forget` \
         (hard delete). All edits are audit-logged."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": { "type": "integer", "description": "Memory id (from retain/recall)" },
                "op": {
                    "type": "string",
                    "enum": ["update", "invalidate", "forget"],
                    "description": "Edit operation"
                },
                "content": { "type": "string", "description": "Replacement content (update only)" }
            },
            "required": ["id", "op"]
        })
    }

    fn effects(&self) -> ToolEffects {
        ToolEffects::write()
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let input: MemoryEditInput =
            serde_json::from_value(input).map_err(|e| Error::validation(e.to_string()))?;
        let op = MemoryEditOp::parse(&input.op)?;
        match self.store.edit(input.id, op, input.content.as_deref()) {
            Ok(()) => {
                let verb = match op {
                    MemoryEditOp::Update => "updated",
                    MemoryEditOp::Invalidate => "invalidated",
                    MemoryEditOp::Forget => "forgotten",
                };
                Ok(text_output(
                    format!("Memory {} {verb}.", input.id),
                    serde_json::json!({
                        "schema": MEMORY_SCHEMA,
                        "id": input.id,
                        "op": input.op,
                    }),
                    false,
                ))
            }
            Err(err) => Ok(text_output(
                err.to_string(),
                serde_json::json!({ "error": err.to_string() }),
                true,
            )),
        }
    }
}

/// `reflect`: synthesize an answer over the bank with citation ids.
pub struct ReflectTool {
    store: Arc<MemoryStore>,
    /// Injectable provider for tests; when None the tool resolves the
    /// session's default provider lazily.
    provider: Option<Arc<dyn crate::provider::Provider>>,
}

impl ReflectTool {
    #[must_use]
    pub fn new(store: Arc<MemoryStore>) -> Self {
        Self {
            store,
            provider: None,
        }
    }

    /// Inject a provider (tests and scripted harnesses).
    #[must_use]
    pub fn with_provider(
        store: Arc<MemoryStore>,
        provider: Arc<dyn crate::provider::Provider>,
    ) -> Self {
        Self {
            store,
            provider: Some(provider),
        }
    }

    /// Union recall over the question's tokens (frequency then recency),
    /// capped — natural questions AND poorly against FTS.
    fn gather(&self, question: &str, cap: usize) -> Result<Vec<Memory>> {
        let mut hits: std::collections::HashMap<i64, (usize, Memory)> =
            std::collections::HashMap::new();
        for token in question.split_whitespace() {
            let token = token.trim_matches(|c: char| !c.is_alphanumeric()); // ubs:ignore punctuation trim, not a secret
            if token.len() < 2 {
                continue;
            }
            for memory in self.store.recall(token, Some(cap))? {
                hits.entry(memory.id)
                    .and_modify(|(count, _)| *count += 1)
                    .or_insert((1, memory));
            }
        }
        let mut ranked: Vec<(usize, Memory)> = hits.into_values().collect();
        ranked.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then(b.1.updated_at_ms.cmp(&a.1.updated_at_ms))
        });
        Ok(ranked
            .into_iter()
            .take(cap)
            .map(|(_, memory)| memory)
            .collect())
    }

    fn resolve_provider(&self) -> Result<Arc<dyn crate::provider::Provider>> {
        if let Some(provider) = &self.provider {
            return Ok(Arc::clone(provider));
        }
        // Lazy resolution (reflect calls are rare): load auth + the model
        // registry and use the session-default (first) model entry, the
        // same fallback the role resolver uses.
        let auth_path = crate::config::Config::global_dir().join("auth.json");
        let auth = crate::auth::AuthStorage::load(auth_path)
            .map_err(|e| Error::tool("reflect", format!("auth load failed: {e}")))?;
        let registry = crate::models::ModelRegistry::load(&auth, None);
        let entry = registry.models().first().ok_or_else(|| {
            Error::tool(
                "reflect",
                "no model available for synthesis (configure a default model)".to_string(),
            )
        })?;
        crate::providers::create_provider(entry, None)
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReflectInput {
    question: String,
}

#[async_trait::async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Tool for ReflectTool {
    fn name(&self) -> &str {
        "reflect"
    }

    fn label(&self) -> &str {
        "reflect"
    }

    fn description(&self) -> &str {
        "Answer a question using this project's memories. Gathers the most \
         relevant memories, synthesizes an answer with the session model, \
         and cites the memory ids used."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "question": { "type": "string", "description": "The question to answer from memory" }
            },
            "required": ["question"]
        })
    }

    fn effects(&self) -> ToolEffects {
        ToolEffects::read()
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let input: ReflectInput =
            serde_json::from_value(input).map_err(|e| Error::validation(e.to_string()))?;
        // Gather top-K: natural questions AND poorly, so recall per token
        // and union (frequency then recency), capped at 8.
        let corpus = self.gather(&input.question, 8)?;
        if corpus.is_empty() {
            return Ok(text_output(
                "No memories to reflect on yet — retain some facts first.".to_string(),
                serde_json::json!({ "schema": MEMORY_SCHEMA, "citations": [] }),
                false,
            ));
        }
        let provider = self.resolve_provider()?;
        let mut prompt = String::from(
            "Answer the question using ONLY the memories below. Cite memory ids in \
             square brackets (e.g. [3]) for every claim. If the memories do not \
             answer the question, say so.\n\nMemories:\n",
        );
        for memory in &corpus {
            let _ = std::fmt::Write::write_fmt(
                &mut prompt,
                format_args!("- [{}] ({}): {}\n", memory.id, memory.kind, memory.content),
            );
        }
        let _ = std::fmt::Write::write_fmt(
            &mut prompt,
            format_args!("\nQuestion: {}\n", input.question),
        );
        let context = Context {
            system_prompt: Some(std::borrow::Cow::Borrowed(
                "You are a precise memory synthesizer. Cite memory ids for every claim.",
            )),
            messages: std::borrow::Cow::Owned(vec![crate::model::Message::User(
                crate::model::UserMessage {
                    content: crate::model::UserContent::Text(prompt),
                    timestamp: now_ms(),
                },
            )]),
            tools: std::borrow::Cow::Borrowed(&[]),
        };
        let options = StreamOptions::default();
        let mut stream = provider.stream(&context, &options).await?;
        let mut answer = String::new();
        while let Some(event) = stream.next().await {
            if let Ok(StreamEvent::TextDelta { delta, .. }) = event {
                answer.push_str(&delta);
            }
        }
        let citations: Vec<i64> = corpus.iter().map(|memory| memory.id).collect();
        let details = serde_json::json!({
            "schema": MEMORY_SCHEMA,
            "question": input.question,
            "citations": citations,
            "memories": corpus,
        });
        Ok(text_output(answer, details, false))
    }
}

use sha2::Digest;

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pi-memory-test-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp root");
        dir
    }

    #[test]
    fn screener_redacts_known_secret_shapes() {
        let screened = screen_secrets("api key = sk-abcdefghijklmnopqrstuvwxyz");
        assert!(
            !screened.contains("sk-abcdef"),
            "secret must be redacted: {screened}"
        );
        assert!(screened.contains("[REDACTED_OPENAI_KEY]"), "{screened}");
        let aws = screen_secrets(concat!("aws key AKIA", "IOSFODNN7EXAMPLE here")); // ubs:ignore AWS documentation example key, not a real secret
        assert!(aws.contains("[REDACTED_AWS_ACCESS_KEY]"), "{aws}");
        let clean = screen_secrets("nothing secret here");
        assert_eq!(clean, "nothing secret here");
    }

    #[test]
    fn retain_recall_invalidate_forget_cycle() {
        let root = temp_root("cycle");
        let store = MemoryStore::open(&root).expect("open");
        let memory = store
            .retain(
                MemoryKind::Fact,
                "the parser lives in src/parser.rs",
                &["layout".to_string()],
                None,
            )
            .expect("retain");
        assert!(memory.id > 0);

        let hits = store.recall("parser", None).expect("recall");
        assert!(
            hits.iter().any(|hit| hit.id == memory.id),
            "recall must find the retained fact: {hits:?}"
        );

        store
            .edit(memory.id, MemoryEditOp::Invalidate, None)
            .expect("invalidate");
        let after = store.recall("parser", None).expect("recall after");
        assert!(
            after.iter().all(|hit| hit.id != memory.id),
            "invalidated memory must be excluded from recall: {after:?}"
        );
        let listed = store.list(50).expect("list");
        assert!(
            listed
                .iter()
                .any(|hit| hit.id == memory.id && hit.status == "invalidated"),
            "tombstone must remain auditable: {listed:?}"
        );

        store
            .edit(memory.id, MemoryEditOp::Forget, None)
            .expect("forget");
        let gone = store.list(50).expect("list after forget");
        assert!(
            gone.iter().all(|hit| hit.id != memory.id),
            "forget must hard-delete: {gone:?}"
        );
    }

    #[test]
    fn duplicate_retain_rejected() {
        let root = temp_root("dupe");
        let store = MemoryStore::open(&root).expect("open");
        store
            .retain(MemoryKind::Fact, "unique fact", &[], None)
            .expect("first");
        let err = store
            .retain(MemoryKind::Fact, "unique fact", &[], None)
            .unwrap_err();
        assert!(
            err.to_string().contains("PI_MEMORY_DUPLICATE"),
            "expected duplicate error: {err}"
        );
    }

    #[test]
    fn project_keys_isolate_stores() {
        let root_a = temp_root("iso-a");
        let root_b = temp_root("iso-b");
        let store_a = MemoryStore::open(&root_a).expect("open a");
        let store_b = MemoryStore::open(&root_b).expect("open b");
        assert_ne!(
            store_a.project_key(),
            store_b.project_key(),
            "different roots must scope different banks"
        );
        store_a
            .retain(MemoryKind::Fact, "alpha-only fact", &[], None)
            .expect("retain");
        let hits_b = store_b.recall("alpha-only", None).expect("recall b");
        assert!(
            hits_b.is_empty(),
            "project B must not see project A's memories: {hits_b:?}"
        );
    }

    #[test]
    fn persistence_across_store_instances() {
        let root = temp_root("persist");
        let id = {
            let store = MemoryStore::open(&root).expect("open first");
            store
                .retain(
                    MemoryKind::Lesson,
                    "always run cargo check first",
                    &[],
                    None,
                )
                .expect("retain")
                .id
        };
        // A fresh store instance (new session) sees the same bank.
        let store = MemoryStore::open(&root).expect("open second");
        let hits = store.recall("cargo check", None).expect("recall");
        assert!(
            hits.iter().any(|hit| hit.id == id),
            "memory must persist across store instances: {hits:?}"
        );
    }

    #[test]
    fn update_replaces_content_and_keeps_id() {
        let root = temp_root("update");
        let store = MemoryStore::open(&root).expect("open");
        let memory = store
            .retain(MemoryKind::Fact, "old wording", &[], None)
            .expect("retain");
        store
            .edit(memory.id, MemoryEditOp::Update, Some("new wording"))
            .expect("update");
        let hits = store.recall("new wording", None).expect("recall");
        assert!(hits.iter().any(|hit| hit.id == memory.id));
        let stale = store.recall("old wording", None).expect("stale recall");
        assert!(stale.iter().all(|hit| hit.id != memory.id));
    }

    #[test]
    fn mental_model_respects_budget() {
        let root = temp_root("budget");
        let store = MemoryStore::open(&root).expect("open");
        for index in 0..30 {
            store
                .retain(
                    MemoryKind::Fact,
                    &format!("fact number {index} {}", "x".repeat(200)),
                    &[],
                    None,
                )
                .expect("retain");
        }
        let model = store.mental_model().expect("mental model");
        assert!(
            model.len() <= MENTAL_MODEL_BUDGET + 64,
            "budget: {}",
            model.len()
        );
        assert!(model.contains("<memory>"));
    }
}
