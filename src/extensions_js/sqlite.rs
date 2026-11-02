//! Synchronous SQLite for PiJS, backed by the existing FrankenSQLite engine.
//!
//! The engine runs on one lazily created, bounded worker per runtime. Database
//! handles belong to the active extension; file-backed databases require a
//! host-granted persistent storage root. SQL cannot acquire another filesystem
//! capability through ATTACH, native functions, virtual-table modules or PRAGMA.
//! Cancellation reaches the engine's actual operation context. A timed-out
//! worker is poisoned and cannot accept further work; the process-wide worker
//! permit stays with the thread until it has really exited.
//! Each synchronous request reserves 30 seconds for processing plus at most
//! one 30-second native lock-wait allowance. Both share the original request
//! start, so batches and retries cannot renew the 60-second wall-time bound.
//! The allowance comes from native PRAGMA state and the pinned engine's
//! open/prepare bootstrap floor; it is not a measurement of CPU time. The
//! embedding actor cannot interrupt this synchronous call with its own timer.
//!
//! Native fsqlite uses a cooperative directory namespace. Guest code has no
//! host symlink/hardlink primitive and cannot name a database outside its grant.
//! We reject links and recheck pinned parents and all companions around each
//! operation. On Unix this does not make fsqlite's pathname-based VFS safe
//! against a separate, same-UID native process replacing directories mid-call.
//!
//! The request, result, page-cache and memory-VFS budgets below do not bound
//! every SQL intermediate allocation. The native file-family limit is checked
//! around operations, not by a hard VFS write quota. A stronger resource or
//! same-UID race boundary requires upstream engine controls or a subprocess.

// The existing engine deliberately exposes !Send, deeply nested futures. They
// are awaited only on the owned worker with its explicit 16 MiB stack.
#![allow(clippy::future_not_send, clippy::large_futures)]

use super::{PiJsModuleState, current_extension_id, persistent_storage_path, storage};
use crate::extensions::{ExtensionPolicy, PolicyDecision};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use fsqlite::{Connection, ConnectionEnv, FrankenError, Row, SqliteValue};
use fsqlite_ast::Statement as SqlStatement;
use fsqlite_parser::Parser;
use rquickjs::Ctx;
use rquickjs::function::Func;
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub(super) const MODULE_SOURCE: &str = include_str!("sqlite.mjs");

const MAX_SQL_BYTES: usize = 64 * 1024;
const MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;
const MAX_VALUE_BYTES: usize = 1024 * 1024;
const MAX_RESULT_BYTES: usize = 8 * 1024 * 1024;
const MAX_RESULT_ROWS: usize = 10_000;
const MAX_PARAMETERS: usize = 999;
const MAX_DATABASES: usize = 16;
const MAX_WORKERS: usize = 8;
const MAX_DATABASE_BYTES: u64 = 128 * 1024 * 1024;
const OPERATION_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_BUSY_TIMEOUT_MS: u64 = 30_000;
// fsqlite 0.4.10 uses this floor for connection admission and prepare refresh,
// independently of a connection's current PRAGMA busy_timeout setting.
const ENGINE_BOOTSTRAP_WAIT: Duration = Duration::from_secs(30);
const CANCEL_GRACE: Duration = Duration::from_millis(250);
static ACTIVE_WORKERS: AtomicUsize = AtomicUsize::new(0);

// The native engine has more companions than libsqlite3. This inventory is
// shared by capability admission, link checks and aggregate size accounting,
// before the first native open as well as around later operations. In
// particular, fsqlite 0.4.10's first-open migration writes its marker and backup
// temporaries using ordinary filesystem calls, even before guest SQL runs.
// Keep their final and temporary paths here; checking only WAL/SHM is unsafe.
const SIDECARS: &[&str] = &[
    "-wal",
    "-shm",
    "-journal",
    "-wal-fec",
    "-wal-fec.lock",
    "-fsqlite-ns-gate",
    "-fsqlite-ns-use",
    "-wal-cert",
    "-wal-cert-head",
    ".fsqlite-shm",
    ".fsqlite-migration-state",
    ".fsqlite-migration-state.tmp",
    ".pre-migration-bak",
    ".pre-migration-bak.tmp",
    ".pre-migration-bak-wal",
    ".pre-migration-bak-wal.tmp",
    ".pre-migration-bak-shm",
    ".pre-migration-bak-shm.tmp",
    "-lock-shared",
    "-lock-reserved",
    "-lock-pending",
    "-wal-lock-shared",
    "-wal-lock-reserved",
    "-wal-lock-pending",
    // Windows opens an advisory-lock triplet for every writable VFS file,
    // including rollback journals and WAL certificates, not only main/WAL.
    "-journal-lock-shared",
    "-journal-lock-reserved",
    "-journal-lock-pending",
    "-wal-cert-lock-shared",
    "-wal-cert-lock-reserved",
    "-wal-cert-lock-pending",
    "-wal-cert-head-lock-shared",
    "-wal-cert-head-lock-reserved",
    "-wal-cert-head-lock-pending",
];

type Result<T> = std::result::Result<T, String>;
type NativeResult<T> = std::result::Result<T, SqliteFailure>;
type Cancel = Arc<dyn Fn() + Send + Sync>;

/// Keep native result codes separate from host admission failures. In
/// particular, context-mode retries by inspecting a message for SQLITE_BUSY;
/// the engine's Display text alone is "database is busy" and is insufficient.
#[derive(Debug, Serialize)]
struct SqliteFailure {
    message: String,
    code: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    errno: Option<i32>,
    #[serde(rename = "primaryCode", skip_serializing_if = "Option::is_none")]
    primary_code: Option<i32>,
}

impl SqliteFailure {
    fn interrupted(message: impl Into<String>) -> Self {
        Self {
            message: format!("SQLITE_INTERRUPT: {}", message.into()),
            code: "SQLITE_INTERRUPT",
            errno: None,
            primary_code: None,
        }
    }

    fn with_context(mut self, context: &str) -> Self {
        self.message.push_str("; ");
        self.message.push_str(context);
        self
    }
}

impl std::fmt::Display for SqliteFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl From<String> for SqliteFailure {
    fn from(message: String) -> Self {
        Self {
            message,
            code: "SQLITE_ERROR",
            errno: None,
            primary_code: None,
        }
    }
}

impl From<&str> for SqliteFailure {
    fn from(message: &str) -> Self {
        message.to_owned().into()
    }
}

impl From<FrankenError> for SqliteFailure {
    fn from(error: FrankenError) -> Self {
        let primary_code = error.error_code() as i32;
        let errno = error.extended_error_code();
        // Only classify using the engine's actual numeric results. SQL,
        // filenames and error prose may themselves contain a code's name.
        let code = match errno {
            261 => "SQLITE_BUSY_RECOVERY",
            517 => "SQLITE_BUSY_SNAPSHOT",
            3091 => "SQLITE_CONSTRAINT_DATATYPE",
            32_526 => "SQLITE_OPEN_NEWER_FORMAT",
            32_270 => "SQLITE_OPEN_INCOMPATIBLE_PEER",
            _ => match primary_code {
                2 => "SQLITE_INTERNAL",
                3 => "SQLITE_PERM",
                4 => "SQLITE_ABORT",
                5 => "SQLITE_BUSY",
                6 => "SQLITE_LOCKED",
                7 => "SQLITE_NOMEM",
                8 => "SQLITE_READONLY",
                9 => "SQLITE_INTERRUPT",
                10 => "SQLITE_IOERR",
                11 => "SQLITE_CORRUPT",
                12 => "SQLITE_NOTFOUND",
                13 => "SQLITE_FULL",
                14 => "SQLITE_CANTOPEN",
                15 => "SQLITE_PROTOCOL",
                16 => "SQLITE_EMPTY",
                17 => "SQLITE_SCHEMA",
                18 => "SQLITE_TOOBIG",
                19 => "SQLITE_CONSTRAINT",
                20 => "SQLITE_MISMATCH",
                21 => "SQLITE_MISUSE",
                22 => "SQLITE_NOLFS",
                23 => "SQLITE_AUTH",
                24 => "SQLITE_FORMAT",
                25 => "SQLITE_RANGE",
                26 => "SQLITE_NOTADB",
                _ => "SQLITE_ERROR",
            },
        };
        Self {
            message: format!("{code}: {error}"),
            code,
            errno: Some(errno),
            primary_code: Some(primary_code),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
enum Bindings {
    Positional(Vec<Json>),
    Named(BTreeMap<String, Json>),
}

impl Default for Bindings {
    fn default() -> Self {
        Self::Positional(Vec::new())
    }
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum QueryMode {
    All,
    Get,
    Values,
    Run,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Open {
        path: String,
        #[serde(default)]
        readonly: bool,
        #[serde(default = "default_create")]
        create: bool,
        #[serde(default)]
        strict: bool,
        #[serde(default)]
        safe_integers: bool,
    },
    Prepare {
        id: u64,
        sql: String,
    },
    Query {
        id: u64,
        sql: String,
        bindings: Bindings,
        mode: QueryMode,
    },
    Exec {
        id: u64,
        sql: String,
    },
    InTransaction {
        id: u64,
    },
    Close {
        id: u64,
    },
}

const fn default_create() -> bool {
    true
}

impl Request {
    const fn database_id(&self) -> Option<u64> {
        match self {
            Self::Open { .. } => None,
            Self::Prepare { id, .. }
            | Self::Query { id, .. }
            | Self::Exec { id, .. }
            | Self::InTransaction { id }
            | Self::Close { id } => Some(*id),
        }
    }
}

#[derive(Debug)]
struct DatabaseOwner {
    extension_id: Option<String>,
    path: Option<PathBuf>,
    readonly: bool,
}

#[derive(Default)]
pub(super) struct SqliteRuntime {
    worker: Option<Worker>,
    databases: HashMap<u64, DatabaseOwner>,
    next_id: u64,
    poisoned: bool,
}

impl SqliteRuntime {
    /// Called before a warm runtime loses its extension identity/storage grants.
    /// An interrupted engine is never recycled as a clean runtime.
    pub(super) fn reset(&mut self) -> Result<()> {
        if self.poisoned {
            return Err("SQLite runtime is retired and cannot be reused".into());
        }
        if let Some(worker) = self.worker.as_mut() {
            worker
                .call(Command::Reset)
                .map_err(|error| error.to_string())?;
        }
        self.worker.take();
        self.databases.clear();
        // Never recycle database ids: a retained JS object must not acquire a
        // new owner's connection after a runtime reset.
        Ok(())
    }

    /// Permanently revoke a quarantined realm without flushing retained writes.
    /// The worker owns cancellation and native cleanup; its process permit is
    /// held until the actual thread exits, even if cancellation is delayed.
    pub(super) fn retire(&mut self) {
        self.poisoned = true;
        self.databases.clear();
        if let Some(mut worker) = self.worker.take() {
            worker.retire();
        }
    }

    fn dispatch(
        &mut self,
        request: Request,
        owner: Option<String>,
        cwd: &Path,
        modules: &Rc<RefCell<PiJsModuleState>>,
        policy: Option<&ExtensionPolicy>,
    ) -> NativeResult<Json> {
        if self.poisoned {
            return Err(SqliteFailure::interrupted(
                "this runtime's SQLite worker is poisoned",
            ));
        }
        if let Some(id) = request.database_id() {
            let db = self
                .databases
                .get(&id)
                .ok_or("SQLite database is closed or belongs to another runtime")?;
            if db.extension_id != owner {
                return Err("SQLite database belongs to another extension".into());
            }
            if let Some(path) = &db.path
                && let Err(error) =
                    authorize_database(path, owner.as_deref(), modules, policy, db.readonly)
            {
                // Once authority or the namespace changes, do not leave
                // an engine handle that could later flush through it.
                if let Some(worker) = self.worker.as_mut() {
                    let _ = worker.call(Command::Retire { id });
                }
                self.databases.remove(&id);
                return Err(error.into());
            }
        }

        let mut opened = None;
        let command = match request {
            Request::Open {
                path,
                readonly,
                create,
                strict,
                safe_integers,
            } => {
                if self.databases.len() >= MAX_DATABASES {
                    return Err("SQLite runtime exceeds its 16-database limit".into());
                }
                let memory = path.is_empty() || path == ":memory:";
                if path.contains('\0') || (!memory && path.starts_with("file:")) {
                    return Err("SQLite filenames must be plain paths, not URI filenames".into());
                }
                let path = if memory {
                    None
                } else {
                    let input = PathBuf::from(path);
                    let path = if input.is_absolute() {
                        input
                    } else {
                        cwd.join(input)
                    };
                    Some(authorize_database(
                        &path,
                        owner.as_deref(),
                        modules,
                        policy,
                        readonly,
                    )?)
                };
                self.next_id = self
                    .next_id
                    .checked_add(1)
                    .ok_or("SQLite database id exhausted")?;
                let id = self.next_id;
                opened = Some((
                    id,
                    DatabaseOwner {
                        extension_id: owner,
                        path: path.clone(),
                        readonly,
                    },
                ));
                Command::Open {
                    id,
                    path,
                    readonly,
                    create,
                    strict,
                    safe_integers,
                }
            }
            other => Command::Request(other),
        };
        let closing = match &command {
            Command::Request(Request::Close { id }) => Some(*id),
            _ => None,
        };
        if self.worker.is_none() {
            self.worker = Some(Worker::start()?);
        }
        let worker = self.worker.as_mut().ok_or("SQLite worker unavailable")?;
        let result = worker.call(command);
        self.poisoned = worker.stopped.load(Ordering::Acquire);
        if result.is_ok() {
            if let Some((id, db)) = opened {
                self.databases.insert(id, db);
            }
            if let Some(id) = closing {
                self.databases.remove(&id);
            }
        }
        result
    }
}

pub(super) fn install(
    ctx: &Ctx<'_>,
    state: Rc<RefCell<SqliteRuntime>>,
    modules: Rc<RefCell<PiJsModuleState>>,
    cwd: String,
    policy: Option<ExtensionPolicy>,
) -> rquickjs::Result<()> {
    ctx.globals().set(
        "__pi_sqlite_native",
        Func::from(move |ctx: Ctx<'_>, wire: String| -> String {
            let result = if wire.len() > MAX_REQUEST_BYTES {
                Err("SQLite request exceeds 8 MiB".into())
            } else {
                serde_json::from_str::<Request>(&wire)
                    .map_err(|error| {
                        SqliteFailure::from(format!("Invalid SQLite request: {error}"))
                    })
                    .and_then(|request| {
                        state.borrow_mut().dispatch(
                            request,
                            current_extension_id(&ctx),
                            Path::new(&cwd),
                            &modules,
                            policy.as_ref(),
                        )
                    })
            };
            match result {
                Ok(value) => json!({ "ok": value }).to_string(),
                Err(error) => json!({ "error": error }).to_string(),
            }
        }),
    )
}

fn companion_paths(path: &Path) -> Vec<PathBuf> {
    let mut paths = SIDECARS
        .iter()
        .map(|suffix| {
            let mut name = path.as_os_str().to_os_string();
            name.push(suffix);
            PathBuf::from(name)
        })
        .collect::<Vec<_>>();
    let mut fec = path.as_os_str().to_os_string();
    fec.push("-wal-fec");
    paths.push(PathBuf::from(fec).with_extension("wal-fec.tmp"));
    paths
}

fn authorize_database(
    path: &Path,
    owner: Option<&str>,
    modules: &Rc<RefCell<PiJsModuleState>>,
    policy: Option<&ExtensionPolicy>,
    readonly: bool,
) -> Result<PathBuf> {
    // A read-only main pager still enters FrankenSQLite's native namespace
    // protocol. Opening an existing or copied database can create or repair
    // coordination files before query_only is enabled. Require authority for
    // those writes before entering the worker, including on read-only opens.
    for capability in ["read", "write"] {
        if policy.is_some_and(|policy| {
            policy.evaluate_for(capability, owner).decision != PolicyDecision::Allow
        }) {
            if readonly && capability == "write" {
                return Err(
                    "SQLite write capability denied: native coordination files require write permission even for readonly connections"
                        .into(),
                );
            }
            return Err(format!("SQLite {capability} capability denied"));
        }
    }
    let path = persistent_storage_path(path, owner, modules)
        .map_err(|error| error.to_string())?
        .ok_or("File-backed SQLite requires a host-granted persistent storage root")?;
    for companion in companion_paths(&path) {
        persistent_storage_path(&companion, owner, modules)
            .map_err(|error| error.to_string())?
            .ok_or("SQLite companion escaped its persistent storage root")?;
    }
    Ok(path)
}

// SQLite has no sandbox-authorizer API in the pinned engine. Admission is a
// deliberately restricted SQL token grammar, not substring matching. Decode
// quoted identifiers, skip comments and string contents, and validate EVERY
// statement before dispatch so a later forbidden statement cannot follow an
// already committed prefix. Engine parsing still owns SQL syntax/semantics.
#[derive(Debug, Clone)]
enum Token {
    Word(String),
    Text(String),
    Parameter(String),
    Symbol(char),
}

impl Token {
    fn name(&self) -> Option<&str> {
        match self {
            Self::Word(value) | Self::Text(value) => Some(value),
            _ => None,
        }
    }
    fn word(&self) -> Option<&str> {
        match self {
            Self::Word(value) => Some(value),
            _ => None,
        }
    }
}

#[derive(Debug)]
struct AdmittedSql {
    statements: Vec<Vec<Token>>,
    parsed: Vec<SqlStatement>,
    parameters: Vec<Option<String>>,
}

impl AdmittedSql {
    fn requested_busy_timeout(&self) -> Duration {
        // Admission has already decoded every quoted identifier and limited
        // assignments/calls to literal integer values. Reserve before a batch
        // can increase its timeout and encounter contention in a later SQL
        // statement; this is not a shadow copy of the connection setting.
        self.statements
            .iter()
            .filter(|tokens| {
                tokens.first().and_then(Token::word) == Some("pragma")
                    && tokens
                        .get(1)
                        .and_then(Token::name)
                        .is_some_and(|name| name.eq_ignore_ascii_case("busy_timeout"))
            })
            .filter_map(|tokens| tokens.get(3).and_then(Token::name))
            .filter_map(|value| value.parse::<u64>().ok())
            .max()
            .map_or(Duration::ZERO, Duration::from_millis)
    }

    fn is_dml(&self) -> bool {
        matches!(
            self.parsed.first(),
            Some(SqlStatement::Insert(_) | SqlStatement::Update(_) | SqlStatement::Delete(_))
        )
    }

    fn uses_native_prepare(&self) -> bool {
        matches!(
            self.parsed.first(),
            Some(
                SqlStatement::Select(_)
                    | SqlStatement::Insert(_)
                    | SqlStatement::Update(_)
                    | SqlStatement::Delete(_)
                    | SqlStatement::Pragma(_)
            )
        )
    }
}

#[allow(clippy::too_many_lines)]
fn sql_tokens(sql: &str) -> Result<Vec<Vec<Token>>> {
    if sql.is_empty() || sql.len() > MAX_SQL_BYTES || sql.contains('\0') {
        return Err("SQL must contain 1 to 65536 bytes and no NUL".into());
    }
    let mut chars = sql.chars().peekable();
    let mut statements = Vec::new();
    let mut current = Vec::new();
    let mut depth = 0_usize;
    let mut count = 0_usize;
    while let Some(ch) = chars.next() {
        if ch.is_ascii_whitespace() {
            continue;
        }
        if ch == '-' && chars.peek() == Some(&'-') {
            chars.next();
            for ch in chars.by_ref() {
                if ch == '\n' || ch == '\r' {
                    break;
                }
            }
            continue;
        }
        if ch == '/' && chars.peek() == Some(&'*') {
            chars.next();
            let mut closed = false;
            while let Some(ch) = chars.next() {
                if ch == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    closed = true;
                    break;
                }
            }
            if !closed {
                return Err("Unterminated SQL comment".into());
            }
            continue;
        }
        if ch == ';' {
            if depth != 0 {
                return Err("Statement separator inside SQL parentheses".into());
            }
            if !current.is_empty() {
                statements.push(std::mem::take(&mut current));
            }
            if statements.len() > 128 {
                return Err("SQL batch exceeds 128 statements".into());
            }
            continue;
        }
        let token = if matches!(ch, '\'' | '"' | '`' | '[') {
            let close = if ch == '[' { ']' } else { ch };
            let mut value = String::new();
            let mut closed = false;
            while let Some(next) = chars.next() {
                if next == close {
                    if close != ']' && chars.peek() == Some(&close) {
                        chars.next();
                        value.push(close);
                    } else {
                        closed = true;
                        break;
                    }
                } else {
                    value.push(next);
                }
            }
            if !closed {
                return Err("Unterminated SQL string or identifier".into());
            }
            if ch == '\'' {
                Token::Text(value)
            } else {
                Token::Word(value.to_ascii_lowercase())
            }
        } else if matches!(ch, '?' | ':' | '@' | '$') {
            let mut value = ch.to_string();
            while let Some(&next) = chars.peek() {
                if (ch == '?' && next.is_ascii_digit())
                    || (ch != '?' && (next.is_alphanumeric() || next == '_'))
                {
                    value.push(next);
                    chars.next();
                } else {
                    break;
                }
            }
            if ch != '?' && value.len() == 1 {
                return Err("Empty named SQL parameter".into());
            }
            // Tcl-style $name::suffix(...) parameters are deliberately not
            // admitted: their lexer rules differ from ordinary named binds.
            if ch == '$' && matches!(chars.peek(), Some(':' | '(')) {
                return Err("Tcl-style SQL parameters are unsupported".into());
            }
            Token::Parameter(value)
        } else if ch.is_alphanumeric() || ch == '_' {
            let mut value = ch.to_string();
            while let Some(&next) = chars.peek() {
                if next.is_alphanumeric() || next == '_' || next == '$' {
                    value.push(next);
                    chars.next();
                } else {
                    break;
                }
            }
            Token::Word(value.to_ascii_lowercase())
        } else {
            if ch == '(' {
                depth += 1;
                if depth > 64 {
                    return Err("SQL nesting exceeds 64 levels".into());
                }
            } else if ch == ')' {
                depth = depth.checked_sub(1).ok_or("Unbalanced SQL parentheses")?;
            } else if !".,+-*/%=<>!|&~".contains(ch) {
                return Err(format!("Unsupported SQL character {ch:?}"));
            }
            Token::Symbol(ch)
        };
        current.push(token);
        count += 1;
        if count > 16_384 {
            return Err("SQL token limit exceeded".into());
        }
    }
    if depth != 0 {
        return Err("Unbalanced SQL parentheses".into());
    }
    if !current.is_empty() {
        statements.push(current);
    }
    if statements.len() > 128 {
        return Err("SQL batch exceeds 128 statements".into());
    }
    Ok(statements)
}

#[allow(clippy::too_many_lines)]
fn admit_sql(sql: &str, single: bool) -> Result<AdmittedSql> {
    let statements = sql_tokens(sql)?;
    if single && statements.len() != 1 {
        return Err("SQLite prepare requires exactly one statement".into());
    }
    let mut parameters = Vec::new();
    let mut named = HashMap::<String, usize>::new();
    for tokens in &statements {
        let first = tokens
            .first()
            .and_then(Token::word)
            .ok_or("SQL must start with a statement keyword")?;
        if !matches!(
            first,
            "select"
                | "values"
                | "with"
                | "insert"
                | "replace"
                | "update"
                | "delete"
                | "create"
                | "alter"
                | "drop"
                | "begin"
                | "commit"
                | "end"
                | "rollback"
                | "savepoint"
                | "release"
                | "pragma"
        ) {
            return Err(format!(
                "SQLite statement {first:?} is not permitted in the extension sandbox"
            ));
        }
        for (index, token) in tokens.iter().enumerate() {
            if let Some(name) = token.name() {
                let name = name.to_ascii_lowercase();
                let identifier = matches!(token, Token::Word(_));
                let callable_string = matches!(tokens.get(index + 1), Some(Token::Symbol('(')));
                if (identifier || callable_string)
                    && (matches!(
                        name.as_str(),
                        "attach"
                            | "detach"
                            | "vacuum"
                            | "load_extension"
                            | "readfile"
                            | "writefile"
                            | "fsdir"
                            | "edit"
                            | "eval"
                            | "shell"
                            | "fts3_tokenizer"
                    ) || name.starts_with("pragma_")
                        || name.starts_with("fsqlite"))
                {
                    return Err(format!(
                        "SQLite operation {name:?} cannot acquire host filesystem authority"
                    ));
                }
                if identifier && name == "trigger" {
                    return Err("SQLite triggers are not supported in extension databases".into());
                }
                if callable_string
                    && matches!(
                        name.as_str(),
                        "randomblob" | "zeroblob" | "printf" | "format"
                    )
                {
                    // These scalar functions can allocate gigabytes from a
                    // tiny request before the engine reaches a checkpoint.
                    // Do not expose them until the engine has a cell budget.
                    return Err(format!(
                        "SQLite allocating function {name:?} is unsupported in PiJS"
                    ));
                }
            }
            if let Token::Parameter(name) = token {
                let ordinal = if name == "?" {
                    parameters.len() + 1
                } else if let Some(index) = name.strip_prefix('?') {
                    index
                        .parse()
                        .map_err(|_| "Invalid numbered SQL parameter")?
                } else if let Some(index) = named.get(name) {
                    *index
                } else {
                    let index = parameters.len() + 1;
                    named.insert(name.clone(), index);
                    index
                };
                if ordinal == 0 || ordinal > MAX_PARAMETERS {
                    return Err("SQL parameter limit exceeded (1 through 999)".into());
                }
                parameters.resize(parameters.len().max(ordinal), None);
                if name != "?" && parameters[ordinal - 1].is_none() {
                    parameters[ordinal - 1] = Some(name.clone());
                }
            }
        }
        if first == "pragma" {
            admit_pragma(tokens)?;
        }
        if first == "create" && tokens.iter().any(|token| token.word() == Some("virtual")) {
            let module = tokens
                .windows(2)
                .find(|pair| pair[0].word() == Some("using"))
                .and_then(|pair| pair[1].name());
            if !module.is_some_and(|module| module.eq_ignore_ascii_case("fts5")) {
                return Err("Only the built-in fts5 virtual-table module is permitted".into());
            }
        }
    }
    if statements.len() > 1 && !parameters.is_empty() {
        return Err("Bound parameters require one SQLite statement".into());
    }
    // The engine's parser is the final authority on statement boundaries and
    // syntax. This also validates DDL at prepare time without executing it:
    // fsqlite 0.4.10's native PreparedStatement supports only DML and PRAGMA.
    let (parsed, errors) = Parser::from_sql(sql).parse_all();
    if let Some(error) = errors.first() {
        return Err(format!("Invalid SQLite SQL: {error}"));
    }
    if parsed.len() != statements.len() {
        return Err("SQLite lexer and parser disagree about statement boundaries".into());
    }
    for statement in &parsed {
        match statement {
            SqlStatement::Begin(begin)
                if matches!(begin.mode, Some(fsqlite_ast::TransactionMode::Concurrent)) =>
            {
                return Err(
                    "BEGIN CONCURRENT is not supported by the bun:sqlite compatibility contract"
                        .into(),
                );
            }
            SqlStatement::Select(_)
            | SqlStatement::Insert(_)
            | SqlStatement::Update(_)
            | SqlStatement::Delete(_)
            | SqlStatement::CreateTable(_)
            | SqlStatement::CreateIndex(_)
            | SqlStatement::CreateView(_)
            | SqlStatement::Drop(_)
            | SqlStatement::AlterTable(_)
            | SqlStatement::Begin(_)
            | SqlStatement::Commit
            | SqlStatement::Rollback(_)
            | SqlStatement::Savepoint(_)
            | SqlStatement::Release(_)
            | SqlStatement::Pragma(_) => {}
            SqlStatement::CreateVirtualTable(table)
                if table.module.eq_ignore_ascii_case("fts5") => {}
            _ => {
                return Err(
                    "SQLite statement is outside the permitted extension SQL grammar".into(),
                );
            }
        }
    }
    Ok(AdmittedSql {
        statements,
        parsed,
        parameters,
    })
}

fn admit_pragma(tokens: &[Token]) -> Result<()> {
    let name = tokens
        .get(1)
        .and_then(Token::name)
        .ok_or("PRAGMA requires a name")?
        .to_ascii_lowercase();
    // A schema qualifier or dotted fsqlite PRAGMA would bypass a name-only
    // allowlist; these are never necessary for an isolated one-file database.
    if matches!(tokens.get(2), Some(Token::Symbol('.'))) {
        return Err("Schema-qualified PRAGMAs are not permitted".into());
    }
    let args = &tokens[2..];
    let value = |allowed: &[&str]| -> Result<()> {
        if args.is_empty() {
            return Ok(());
        }
        let scalar = match args {
            [Token::Symbol('='), token] | [Token::Symbol('('), token, Token::Symbol(')')] => {
                token.name()
            }
            _ => None,
        }
        .ok_or("PRAGMA requires a literal scalar argument")?;
        if allowed
            .iter()
            .any(|value| scalar.eq_ignore_ascii_case(value))
        {
            Ok(())
        } else {
            Err(format!("Unsupported value for PRAGMA {name}"))
        }
    };
    match name.as_str() {
        "journal_mode" => value(&["wal", "delete"]),
        "synchronous" => value(&["normal", "full", "extra", "1", "2", "3"]),
        "foreign_keys" | "defer_foreign_keys" => value(&["on", "off", "0", "1"]),
        "wal_checkpoint" => value(&["passive", "full", "restart", "truncate"]),
        "busy_timeout" => {
            if args.is_empty() {
                return Ok(());
            }
            let scalar = match args {
                [Token::Symbol('='), token] | [Token::Symbol('('), token, Token::Symbol(')')] => {
                    token.name()
                }
                _ => None,
            }
            .and_then(|value| value.parse::<u64>().ok());
            if scalar.is_some_and(|value| value <= MAX_BUSY_TIMEOUT_MS) {
                Ok(())
            } else {
                Err("SQLite busy_timeout must be between 0 and 30000 milliseconds".into())
            }
        }
        "table_info" | "table_xinfo" | "index_list" | "index_info" | "index_xinfo"
        | "foreign_key_list" => {
            if matches!(
                args,
                [
                    Token::Symbol('('),
                    Token::Word(_) | Token::Text(_),
                    Token::Symbol(')')
                ]
            ) {
                Ok(())
            } else {
                Err("Schema inspection PRAGMA requires one literal object name".into())
            }
        }
        "user_version" => {
            if args.is_empty() {
                return Ok(());
            }
            if matches!(args, [Token::Symbol('='), Token::Word(value)] if value.parse::<i32>().is_ok())
            {
                Ok(())
            } else {
                Err("user_version requires an integer literal".into())
            }
        }
        "database_list" | "schema_version" | "page_count" | "freelist_count"
        | "compile_options" => {
            if args.is_empty() {
                Ok(())
            } else {
                Err(format!(
                    "PRAGMA {name} is read-only in the extension sandbox"
                ))
            }
        }
        _ => Err(format!(
            "PRAGMA {name:?} is unsupported in the extension sandbox"
        )),
    }
}

enum Command {
    Open {
        id: u64,
        path: Option<PathBuf>,
        readonly: bool,
        create: bool,
        strict: bool,
        safe_integers: bool,
    },
    Request(Request),
    Retire {
        id: u64,
    },
    Reset,
}

struct Work {
    command: Command,
    budget: Arc<OperationBudget>,
    reply: mpsc::SyncSender<WorkCompletion>,
}

struct WorkCompletion {
    result: NativeResult<Json>,
    finished_at: Instant,
}

/// Reserve one native lock-wait allowance without renewing the request clock.
/// The mutex orders admission against host timeout: a worker cannot publish a
/// new allowance after the caller has already decided to cancel the request.
struct OperationBudget {
    started: Instant,
    processing: Duration,
    hard_timeout: Option<Duration>,
    state: Mutex<OperationBudgetState>,
}

#[derive(Default)]
struct OperationBudgetState {
    lock_wait: Duration,
    expired: bool,
}

impl OperationBudget {
    fn new(processing: Duration, hard_timeout: Option<Duration>) -> Self {
        Self {
            started: Instant::now(),
            processing,
            hard_timeout,
            state: Mutex::new(OperationBudgetState::default()),
        }
    }

    fn total(&self, state: &OperationBudgetState) -> Duration {
        let reserved = self.processing.saturating_add(state.lock_wait);
        self.hard_timeout
            .map_or(reserved, |hard| reserved.min(hard))
    }

    fn reserve_lock_wait(&self, wait: Duration) -> NativeResult<()> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.expired || self.started.elapsed() >= self.total(&state) {
            state.expired = true;
            return Err(SqliteFailure::interrupted(
                "SQLite request deadline elapsed before native execution",
            ));
        }
        state.lock_wait = state
            .lock_wait
            .max(wait.min(Duration::from_millis(MAX_BUSY_TIMEOUT_MS)));
        Ok(())
    }

    fn reserve_database(&self, database: &EngineDatabase, prepare: bool) -> NativeResult<()> {
        let milliseconds = u64::try_from(database.conn.pragma_state().busy_timeout_ms).unwrap_or(0);
        let mut wait = Duration::from_millis(milliseconds);
        if prepare && database.parent.is_some() {
            wait = wait.max(ENGINE_BOOTSTRAP_WAIT);
        }
        self.reserve_lock_wait(wait)
    }

    fn remaining(&self) -> Duration {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.total(&state).saturating_sub(self.started.elapsed())
    }

    fn expire_at(&self, now: Instant) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if now.saturating_duration_since(self.started) >= self.total(&state) {
            state.expired = true;
        }
        state.expired
    }

    fn timeout_error(&self) -> SqliteFailure {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        SqliteFailure::interrupted(format!(
            "SQLite request exceeded {} milliseconds ({} milliseconds processing, {} milliseconds reserved native lock wait); connection and runtime are poisoned",
            self.total(&state).as_millis(),
            self.processing.as_millis(),
            state.lock_wait.as_millis(),
        ))
    }
}

struct WorkerPermit;

impl Drop for WorkerPermit {
    fn drop(&mut self) {
        ACTIVE_WORKERS.fetch_sub(1, Ordering::AcqRel);
    }
}

struct Worker {
    sender: Option<mpsc::SyncSender<Work>>,
    thread: Option<JoinHandle<()>>,
    cancel: Arc<Mutex<Option<Cancel>>>,
    stopped: Arc<AtomicBool>,
    discard_on_stop: Arc<AtomicBool>,
}

impl Worker {
    fn start() -> Result<Self> {
        ACTIVE_WORKERS
            .try_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < MAX_WORKERS).then_some(active + 1)
            })
            .map_err(|_| "SQLite process worker limit reached")?;
        let permit = WorkerPermit;
        let (sender, receiver) = mpsc::sync_channel::<Work>(1);
        let cancel = Arc::new(Mutex::new(None::<Cancel>));
        let stopped = Arc::new(AtomicBool::new(false));
        let discard_on_stop = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let worker_stopped = Arc::clone(&stopped);
        let worker_discard_on_stop = Arc::clone(&discard_on_stop);
        let thread = std::thread::Builder::new()
            .name("pijs-sqlite".into())
            .stack_size(16 * 1024 * 1024)
            .spawn(move || {
                let _permit = permit;
                let mut databases = HashMap::<u64, EngineDatabase>::new();
                while let Ok(work) = receiver.recv() {
                    if worker_stopped.load(Ordering::Acquire) {
                        break;
                    }
                    let result = execute_work(
                        work.command,
                        &mut databases,
                        &worker_cancel,
                        &worker_stopped,
                        &work.budget,
                    );
                    worker_cancel
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .take();
                    let _ = work.reply.send(WorkCompletion {
                        result,
                        finished_at: Instant::now(),
                    });
                    if worker_stopped.load(Ordering::Acquire) {
                        break;
                    }
                }
                // The worker owns every !Send engine and its cleanup. Timeout
                // and quarantine never drop engines on the QuickJS thread.
                for (_, mut database) in databases {
                    database.conn.root_cx().cancel();
                    if !worker_discard_on_stop.load(Ordering::Acquire)
                        && database.verify_paths().is_ok()
                    {
                        let _ = futures::executor::block_on(
                            database.conn.close_without_checkpoint_in_place(),
                        );
                    }
                    // Deadline, quarantine and namespace failures must not
                    // enter async close with its independent teardown Cx: even the
                    // no-checkpoint variant can commit retained writes.
                    // Connection::drop cancels/reclaims state without SQL
                    // commit, rollback or checkpoint I/O.
                }
            })
            .map_err(|error| format!("Cannot start SQLite worker: {error}"))?;
        Ok(Self {
            sender: Some(sender),
            thread: Some(thread),
            cancel,
            stopped,
            discard_on_stop,
        })
    }

    fn call(&mut self, command: Command) -> NativeResult<Json> {
        self.call_with_budget(command, OPERATION_TIMEOUT, None)
    }

    #[cfg(test)]
    fn call_with_timeout(&mut self, command: Command, timeout: Duration) -> NativeResult<Json> {
        self.call_with_budget(command, timeout, Some(timeout))
    }

    fn call_with_budget(
        &mut self,
        command: Command,
        processing: Duration,
        hard_timeout: Option<Duration>,
    ) -> NativeResult<Json> {
        if self.stopped.load(Ordering::Acquire) {
            return Err(SqliteFailure::interrupted("SQLite worker is poisoned"));
        }
        let budget = Arc::new(OperationBudget::new(processing, hard_timeout));
        let (reply, response) = mpsc::sync_channel(1);
        if self
            .sender
            .as_ref()
            .ok_or("SQLite worker is closed")?
            .send(Work {
                command,
                budget: Arc::clone(&budget),
                reply,
            })
            .is_err()
        {
            self.stopped.store(true, Ordering::Release);
            return Err("SQLite worker terminated".into());
        }
        loop {
            match response.recv_timeout(budget.remaining()) {
                Ok(completion) => {
                    // A descheduled caller must not reject a result the engine
                    // completed within budget, or accept a late completion.
                    if !budget.expire_at(completion.finished_at) {
                        return completion.result;
                    }
                    break;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.stopped.store(true, Ordering::Release);
                    return Err("SQLite worker terminated while executing a statement".into());
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    // Native admission may have reserved its one allowance
                    // while this receiver was waiting on the processing
                    // budget. Re-read the same anchored deadline, not a new
                    // relative timeout and never a fresh retry budget.
                    if budget.expire_at(Instant::now()) {
                        break;
                    }
                }
            }
        }
        self.retire();
        // Cooperative engine checkpoints acknowledge cancellation. Never join
        // an unacknowledged thread on the UI path. Its permit remains live
        // until the thread really exits, bounding detached work process-wide.
        let _ = response.recv_timeout(CANCEL_GRACE);
        Err(budget.timeout_error())
    }

    fn retire(&mut self) {
        // Publish the no-flush decision before cancellation can wake the
        // worker into its terminal cleanup. Normal Drop remains graceful.
        self.discard_on_stop.store(true, Ordering::Release);
        self.stop();
    }

    fn stop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(cancel) = self
            .cancel
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            cancel();
        }
        self.sender.take();
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop();
        if let Some(thread) = self.thread.take()
            && thread.is_finished()
        {
            let _ = thread.join();
        }
    }
}

struct EngineDatabase {
    conn: Connection,
    // Drop/close the engine before releasing its directory guards.
    parent: Option<storage::StorageParent>,
    strict: bool,
    safe_integers: bool,
    schema_version: Cell<Option<i64>>,
    namespace_valid: Cell<bool>,
    poisoned: Cell<bool>,
}

impl EngineDatabase {
    fn verify_paths(&self) -> Result<()> {
        if self.poisoned.get() {
            return Err(
                "SQLITE_ABORT: SQLite connection is unusable after failed statement cleanup".into(),
            );
        }
        if !self.namespace_valid.get() {
            return Err("SQLite database namespace was invalidated".into());
        }
        let result = self.parent.as_ref().map_or(Ok(()), |parent| {
            parent
                .verify()
                .map_err(|error| format!("SQLite namespace changed: {error}"))?;
            verify_database_family(parent.path())
        });
        if result.is_err() {
            self.namespace_valid.set(false);
            self.conn.root_cx().cancel();
        }
        result
    }
}

fn verify_database_family(path: &Path) -> Result<()> {
    let mut total = 0_u64;
    for candidate in std::iter::once(path.to_path_buf()).chain(companion_paths(path)) {
        // This repeats no-follow/reparse/hardlink checks immediately before
        // the native engine can open or checkpoint any companion.
        storage::pin_parent(&candidate, false)
            .map_err(|error| format!("Unsafe SQLite file: {error}"))?
            .verify()
            .map_err(|error| error.to_string())?;
        match std::fs::symlink_metadata(&candidate) {
            Ok(metadata) => {
                if !metadata.is_file() {
                    return Err("SQLite paths must name regular files".into());
                }
                total = total
                    .checked_add(metadata.len())
                    .ok_or("SQLite size overflow")?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    if total > MAX_DATABASE_BYTES {
        return Err("SQLite database and companions exceed 128 MiB".into());
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn execute_work(
    command: Command,
    databases: &mut HashMap<u64, EngineDatabase>,
    cancel: &Mutex<Option<Cancel>>,
    stopped: &AtomicBool,
    budget: &OperationBudget,
) -> NativeResult<Json> {
    match command {
        Command::Open {
            id,
            path,
            readonly,
            create,
            strict,
            safe_integers,
        } => {
            if path.is_some() {
                budget.reserve_lock_wait(ENGINE_BOOTSTRAP_WAIT)?;
            }
            let parent = path
                .as_deref()
                .map(|path| {
                    verify_database_family(path)?;
                    storage::pin_parent(path, false).map_err(|error| error.to_string())
                })
                .transpose()?;
            let path = path
                .as_deref()
                .map(|path| {
                    path.to_str()
                        .map(str::to_owned)
                        .ok_or("SQLite native path is not Unicode")
                })
                .transpose()?
                .unwrap_or_else(|| ":memory:".into());
            let defaults = ConnectionEnv::default();
            let open_cx = defaults.runtime().root_cx().create_child();
            let cancel_cx = open_cx.clone();
            set_cancel(cancel, stopped, Arc::new(move || cancel_cx.cancel()));
            let mut env =
                ConnectionEnv::new_with_root_cx(fsqlite::RuntimeConfig::default(), &open_cx);
            env.mark_blocking_io_inline_safe();
            env.set_strict_multi_process(true);
            env.set_page_buffer_max(8192);
            env.set_memory_vfs_config(fsqlite::fsqlite_vfs::MemoryVfsConfig {
                max_bytes: Some(128 * 1024 * 1024),
                ..Default::default()
            });
            let conn = futures::executor::block_on(async {
                if readonly && path != ":memory:" {
                    // readonly always means existing-only; context-mode's
                    // adapter also passes create:true for read-only opens.
                    Connection::open_schema_only_with_env(path, env).await
                } else if !create && path != ":memory:" {
                    Connection::open_existing_with_env(path, env).await
                } else {
                    Connection::open_with_env(path, env).await
                }
            })
            .map_err(SqliteFailure::from)?;
            futures::executor::block_on(async {
                // Bun and stock SQLite peers share a single-writer WAL
                // contract. The engine otherwise defaults bare BEGIN,
                // top-level SAVEPOINT and autocommit writes to its own MVCC
                // mode. Keep that engine-specific choice host-owned so a
                // context-mode MCP child sees ordinary SQLite lock behavior.
                conn.execute("PRAGMA fsqlite.concurrent_mode = OFF").await?;
                // Temporary tables remain in the engine's memory store. The
                // guest cannot change either setting through PRAGMA. Schema
                // admission remains mandatory: trusted_schema is not a SQL
                // authorizer in the pinned engine.
                conn.execute_batch("PRAGMA trusted_schema = OFF; PRAGMA temp_store = MEMORY")
                    .await?;
                if readonly {
                    conn.execute("PRAGMA query_only = ON").await?;
                }
                Ok::<_, FrankenError>(())
            })
            .map_err(SqliteFailure::from)?;
            let database = EngineDatabase {
                conn,
                parent,
                strict,
                safe_integers,
                schema_version: Cell::new(None),
                namespace_valid: Cell::new(true),
                poisoned: Cell::new(false),
            };
            database.verify_paths()?;
            futures::executor::block_on(validate_schema(&database, budget))?;
            databases.insert(id, database);
            Ok(json!({ "id": id }))
        }
        Command::Request(Request::Close { id }) => {
            let database = databases.get_mut(&id).ok_or("SQLite database is closed")?;
            database.verify_paths()?;
            budget.reserve_database(database, false)?;
            // Close cannot borrow a binding guard and &mut Connection at once;
            // cancelling this terminal operation cancels the connection root.
            let cx = database.conn.root_cx().clone();
            set_cancel(cancel, stopped, Arc::new(move || cx.cancel()));
            futures::executor::block_on(database.conn.close_in_place())
                .map_err(SqliteFailure::from)?;
            database.verify_paths()?;
            databases.remove(&id);
            Ok(Json::Null)
        }
        Command::Request(request) => {
            let id = request
                .database_id()
                .ok_or("Invalid SQLite worker request")?;
            let database = databases.get(&id).ok_or("SQLite database is closed")?;
            database.verify_paths()?;
            let operation_cx = database.conn.root_cx().create_child();
            let cancel_cx = operation_cx.clone();
            set_cancel(cancel, stopped, Arc::new(move || cancel_cx.cancel()));
            let binding = database.conn.bind_operation_cx(&operation_cx);
            let result = futures::executor::block_on(async {
                validate_schema(database, budget).await?;
                execute_request(database, request, budget).await
            });
            drop(binding);
            if database.poisoned.get() {
                // Cleanup already failed and cancelled the connection. Keep
                // its precise error, and do not perform further path/I/O work.
                return result;
            }
            database.verify_paths()?;
            result
        }
        Command::Retire { id } => {
            if let Some(database) = databases.remove(&id) {
                database.conn.root_cx().cancel();
                // Deliberately use non-I/O Drop, never close/flush, after
                // filesystem or capability authority has been revoked.
            }
            Ok(Json::Null)
        }
        Command::Reset => {
            let mut first_error: Option<SqliteFailure> = None;
            for (_, mut database) in databases.drain() {
                if let Err(error) = database.verify_paths() {
                    first_error.get_or_insert_with(|| error.into());
                    continue;
                }
                if let Err(error) = budget.reserve_database(&database, false) {
                    first_error.get_or_insert(error);
                    continue;
                }
                let cx = database.conn.root_cx().clone();
                set_cancel(cancel, stopped, Arc::new(move || cx.cancel()));
                if let Err(error) = futures::executor::block_on(database.conn.close_in_place()) {
                    first_error.get_or_insert_with(|| error.into());
                }
                if let Err(error) = database.verify_paths() {
                    first_error.get_or_insert_with(|| error.into());
                }
            }
            first_error.map_or(Ok(Json::Null), Err)
        }
    }
}

fn set_cancel(slot: &Mutex<Option<Cancel>>, stopped: &AtomicBool, cancel: Cancel) {
    let mut slot = slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Cover the handoff race where the caller's deadline expired during path
    // preflight, before the engine had published its cancellation context.
    if stopped.load(Ordering::Acquire) {
        cancel();
    }
    *slot = Some(cancel);
}

async fn validate_schema(database: &EngineDatabase, budget: &OperationBudget) -> NativeResult<()> {
    budget.reserve_database(database, false)?;
    let rows = database
        .conn
        .query("PRAGMA schema_version")
        .await
        .map_err(SqliteFailure::from)?;
    let version = rows
        .first()
        .and_then(|row| row.get(0))
        .and_then(|value| {
            if let SqliteValue::Integer(value) = value {
                Some(*value)
            } else {
                None
            }
        })
        .ok_or("SQLite schema version is unavailable")?;
    if database.schema_version.get() == Some(version) {
        return Ok(());
    }
    budget.reserve_database(database, true)?;
    let rows = database
        .conn
        .query("SELECT type, sql FROM sqlite_master WHERE sql IS NOT NULL LIMIT 1025")
        .await
        .map_err(SqliteFailure::from)?;
    if rows.len() > 1024 {
        return Err("SQLite schema exceeds 1024 objects".into());
    }
    for row in rows {
        if matches!(row.get(0), Some(SqliteValue::Text(kind)) if kind.as_ref() == "trigger") {
            return Err("Existing SQLite triggers are not permitted in extension databases".into());
        }
        let Some(SqliteValue::Text(sql)) = row.get(1) else {
            return Err("Invalid SQLite schema text".into());
        };
        admit_sql(sql.as_ref(), true)?;
    }
    database.schema_version.set(Some(version));
    Ok(())
}

#[allow(clippy::too_many_lines)]
async fn execute_request(
    database: &EngineDatabase,
    request: Request,
    budget: &OperationBudget,
) -> NativeResult<Json> {
    match request {
        Request::Prepare { sql, .. } => {
            let admitted = admit_sql(&sql, true)?;
            let columns = if admitted.uses_native_prepare() {
                budget.reserve_database(database, true)?;
                database
                    .conn
                    .prepare(&sql)
                    .await
                    .map_err(SqliteFailure::from)?
                    .column_names()
            } else {
                // DDL/control syntax was parsed above, with no mutations.
                // Those statements produce no result columns; native engine
                // semantic validation happens when the statement is stepped.
                Vec::new()
            };
            Ok(json!({ "columns": columns, "parameters": admitted.parameters.len() }))
        }
        Request::Query {
            sql,
            bindings,
            mode,
            ..
        } => {
            let admitted = admit_sql(&sql, true)?;
            budget.reserve_lock_wait(admitted.requested_busy_timeout())?;
            let params = bind_parameters(&admitted.parameters, bindings, database.strict)?;
            if !admitted.uses_native_prepare() {
                if params.is_empty() {
                    // execute() retains the original DDL source for default
                    // expressions and CHECK constraints. The parameterized
                    // dispatcher does not publish that source in 0.4.10.
                    database
                        .conn
                        .execute(&sql)
                        .await
                        .map_err(SqliteFailure::from)?;
                } else {
                    database
                        .conn
                        .execute_with_params(&sql, &params)
                        .await
                        .map_err(SqliteFailure::from)?;
                }
                return if matches!(mode, QueryMode::Run) {
                    change_result(database, budget).await
                } else {
                    Ok(json!({ "columns": [], "rows": [] }))
                };
            }
            budget.reserve_database(database, true)?;
            let statement = database
                .conn
                .prepare(&sql)
                .await
                .map_err(SqliteFailure::from)?;
            if matches!(mode, QueryMode::Run) {
                statement
                    .execute_with_params(&params)
                    .await
                    .map_err(SqliteFailure::from)?;
                return change_result(database, budget).await;
            }
            let columns = statement.column_names();
            let mut bytes = serde_json::to_vec(&columns)
                .map_err(|error| error.to_string())?
                .len()
                + 64;
            if columns.len() > 256 || bytes > MAX_RESULT_BYTES {
                return Err("SQLite result has too many columns".into());
            }
            let is_dml = admitted.is_dml();
            if is_dml {
                // The pinned engine materializes RETURNING before invoking
                // row callbacks. Keep its writes uncommitted until every
                // result has passed the bridge's row/encoding limits.
                database
                    .conn
                    .execute("SAVEPOINT __pijs_row_result")
                    .await
                    .map_err(SqliteFailure::from)?;
            }
            let mut rows = Vec::new();
            let mut seen = 0_usize;
            let mut limit_error = None;
            let mut stopped_after_first = false;
            let mut accept = |row: &Row| -> std::result::Result<(), FrankenError> {
                seen += 1;
                if seen > MAX_RESULT_ROWS || budget.expire_at(Instant::now()) {
                    limit_error = Some("SQLite result exceeds the row or time limit".to_string());
                    return Err(FrankenError::Abort);
                }
                if matches!(mode, QueryMode::Get) && !rows.is_empty() {
                    return Ok(());
                }
                bytes += 3; // enclosing row brackets and its separator
                match row
                    .values()
                    .iter()
                    .map(|value| encode_value(value, database.safe_integers, &mut bytes))
                    .collect::<Result<Vec<_>>>()
                {
                    Ok(row) => {
                        rows.push(row);
                        if matches!(mode, QueryMode::Get) && !is_dml {
                            // A read-only get() needs one row. The engine's
                            // callback-abort path closes only its read cursor
                            // and preserves an enclosing transaction.
                            stopped_after_first = true;
                            return Err(FrankenError::Abort);
                        }
                        Ok(())
                    }
                    Err(error) => {
                        limit_error = Some(error);
                        Err(FrankenError::Abort)
                    }
                }
            };
            // PreparedStatement's query API rejects DML, including RETURNING.
            // The connection query dispatcher is its actual DML row interface.
            // Consume all DML rows even for get(), retaining only the first.
            let mut preserve_native_constraint = false;
            let outcome = if is_dml {
                // This API also applies the engine's DQS rewrites and error
                // offsets. Its for_each variant omits those wrappers, while
                // materializing DML rows in exactly the same way.
                match database.conn.query_with_params(&sql, &params).await {
                    Ok(native_rows) => native_rows.iter().try_for_each(&mut accept),
                    Err(error) => {
                        preserve_native_constraint = is_native_constraint_error(&error);
                        Err(error)
                    }
                }
            } else {
                statement
                    .query_with_params_for_each(&params, &mut accept)
                    .await
            };
            let outcome = match (outcome, limit_error) {
                (_, Some(error)) => Err(error.into()),
                (Err(FrankenError::Abort), None) if stopped_after_first => Ok(()),
                (result, None) => result.map_err(SqliteFailure::from),
            };
            if is_dml {
                finish_row_savepoint(database, outcome, preserve_native_constraint).await?;
            } else {
                outcome?;
            }
            Ok(json!({ "columns": columns, "rows": rows }))
        }
        Request::Exec { sql, .. } => {
            let admitted = admit_sql(&sql, false)?;
            if !admitted.parameters.is_empty() {
                return Err("Use a prepared statement for SQL parameters".into());
            }
            budget.reserve_lock_wait(admitted.requested_busy_timeout())?;
            let may_prepare = admitted.parsed.iter().any(|statement| {
                !matches!(
                    statement,
                    SqlStatement::Begin(_)
                        | SqlStatement::Commit
                        | SqlStatement::Rollback(_)
                        | SqlStatement::Savepoint(_)
                        | SqlStatement::Release(_)
                        | SqlStatement::Pragma(_)
                )
            });
            budget.reserve_database(database, may_prepare)?;
            if !admitted.statements.is_empty() {
                database
                    .conn
                    .execute_batch(&sql)
                    .await
                    .map_err(SqliteFailure::from)?;
            }
            change_result(database, budget).await
        }
        Request::InTransaction { .. } => Ok(json!(database.conn.in_transaction())),
        Request::Open { .. } | Request::Close { .. } => Err("Invalid SQLite operation".into()),
    }
}

fn is_native_constraint_error(error: &FrankenError) -> bool {
    // Mirror the pinned engine's error_is_constraint_violation, including its
    // VDBE fallback. 19 is SQLite's stable primary SQLITE_CONSTRAINT code.
    error.error_code() as i32 == 19
        || matches!(error, FrankenError::Internal(message) if message.starts_with("VDBE halted with code 19:"))
}

async fn finish_row_savepoint(
    database: &EngineDatabase,
    outcome: NativeResult<()>,
    preserve_native_constraint: bool,
) -> NativeResult<()> {
    let failure = match outcome {
        Ok(()) => match database.conn.execute("RELEASE __pijs_row_result").await {
            Ok(_) => return Ok(()),
            Err(error) => SqliteFailure::from(error),
        },
        Err(error) if preserve_native_constraint => {
            // The engine has already applied ABORT/FAIL/ROLLBACK policy.
            // FAIL deliberately retains a successful row prefix; don't undo
            // it merely because this bridge owns an additional savepoint.
            if !database.conn.in_transaction() {
                return Err(error);
            }
            match database.conn.execute("RELEASE __pijs_row_result").await {
                Ok(_) => return Err(error),
                Err(release) => {
                    error.with_context(&format!("SQLite statement release failed: {release}"))
                }
            }
        }
        Err(error) => error,
    };
    // ON CONFLICT ROLLBACK may already have ended the complete transaction.
    // Otherwise roll back just this host-owned savepoint, preserving caller
    // writes that preceded it in an explicit transaction.
    if database.conn.in_transaction()
        && let Err(error) = database
            .conn
            .execute_batch("ROLLBACK TO __pijs_row_result; RELEASE __pijs_row_result")
            .await
    {
        database.poisoned.set(true);
        database.conn.root_cx().cancel();
        return Err(failure.with_context(&format!("SQLite statement rollback failed: {error}")));
    }
    Err(failure)
}

async fn change_result(database: &EngineDatabase, budget: &OperationBudget) -> NativeResult<Json> {
    budget.reserve_database(database, true)?;
    let rows = database
        .conn
        .query("SELECT changes()")
        .await
        .map_err(SqliteFailure::from)?;
    let changes = rows
        .first()
        .and_then(|row| row.get(0))
        .and_then(|value| {
            if let SqliteValue::Integer(value) = value {
                Some(*value)
            } else {
                None
            }
        })
        .ok_or("SQLite did not report the change count")?;
    let mut bytes = 0;
    Ok(
        json!({ "changes": changes, "lastInsertRowid": encode_value(&SqliteValue::Integer(database.conn.last_insert_rowid()), database.safe_integers, &mut bytes)? }),
    )
}

fn bind_parameters(
    parameters: &[Option<String>],
    bindings: Bindings,
    strict: bool,
) -> Result<Vec<SqliteValue>> {
    let values = match bindings {
        Bindings::Positional(mut values) => {
            if values.len() > parameters.len() || (strict && values.len() != parameters.len()) {
                return Err(format!(
                    "SQLite expected {} parameters, received {}",
                    parameters.len(),
                    values.len()
                ));
            }
            values.resize(parameters.len(), Json::Null);
            values
        }
        Bindings::Named(values) => {
            if values.len() > MAX_PARAMETERS {
                return Err("Too many named SQL parameters".into());
            }
            parameters
                .iter()
                .map(|name| {
                    let value = name.as_ref().and_then(|name| {
                        values
                            .get(name)
                            .or_else(|| strict.then(|| values.get(&name[1..])).flatten())
                    });
                    if strict && value.is_none() {
                        return Err(format!("Missing SQLite parameter {name:?}"));
                    }
                    Ok(value.cloned().unwrap_or(Json::Null))
                })
                .collect::<Result<Vec<_>>>()?
        }
    };
    values.into_iter().map(decode_value).collect()
}

fn decode_value(value: Json) -> Result<SqliteValue> {
    match value {
        Json::Null => Ok(SqliteValue::Null),
        Json::Bool(value) => Ok(SqliteValue::Integer(i64::from(value))),
        Json::Number(value) => {
            if let Some(value) = value.as_i64() { Ok(SqliteValue::Integer(value)) }
            else { value.as_f64().filter(|value| value.is_finite()).map(SqliteValue::Float).ok_or("Invalid SQLite number".into()) }
        }
        Json::String(value) if value.len() <= MAX_VALUE_BYTES => Ok(SqliteValue::Text(value.into())),
        Json::Object(value) if value.len() == 1 => {
            if let Some(value) = value.get("$sqliteInteger").and_then(Json::as_str) {
                return value.parse::<i64>().map(SqliteValue::Integer).map_err(|_| "SQLite integer is outside the signed 64-bit range".into());
            }
            if let Some(value) = value.get("$sqliteBlob").and_then(Json::as_str) {
                if value.len() > MAX_VALUE_BYTES.saturating_mul(4).div_ceil(3) + 4 { return Err("SQLite blob exceeds 1 MiB".into()); }
                let bytes = BASE64.decode(value).map_err(|error| error.to_string())?;
                if bytes.len() > MAX_VALUE_BYTES { return Err("SQLite blob exceeds 1 MiB".into()); }
                return Ok(SqliteValue::Blob(bytes.into()));
            }
            Err("Unsupported SQLite parameter object".into())
        }
        _ => Err("SQLite parameters must be null, booleans, numbers, strings, bigint or byte arrays (at most 1 MiB each)".into()),
    }
}

fn encode_value(value: &SqliteValue, integers: bool, bytes: &mut usize) -> Result<Json> {
    let raw_size = match value {
        SqliteValue::Text(value) => value.len(),
        SqliteValue::Blob(value) => value.len(),
        _ => 32,
    };
    if raw_size > MAX_VALUE_BYTES {
        return Err("SQLite result cell exceeds 1 MiB".into());
    }
    let encoded = match value {
        SqliteValue::Null => Json::Null,
        SqliteValue::Integer(value) if integers => {
            json!({ "$sqliteInteger": value.to_string() })
        }
        SqliteValue::Integer(value) => json!(value),
        SqliteValue::Float(value) => serde_json::Number::from_f64(*value)
            .map(Json::Number)
            .ok_or("SQLite returned a non-finite number")?,
        SqliteValue::Text(value) => Json::String(value.to_string()),
        SqliteValue::Blob(value) => json!({ "$sqliteBlob": BASE64.encode(value) }),
    };
    // Count actual wire bytes: control characters need JSON escaping and
    // blobs need base64. Raw value lengths would undercount by up to 6x.
    *bytes = bytes
        .checked_add(encoded.to_string().len() + 1)
        .ok_or("SQLite result size overflow")?;
    if *bytes > MAX_RESULT_BYTES {
        return Err("SQLite result exceeds 8 MiB".into());
    }
    Ok(encoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_file(worker: &mut Worker, id: u64, path: &Path) {
        worker
            .call(Command::Open {
                id,
                path: Some(path.to_path_buf()),
                readonly: false,
                create: true,
                strict: false,
                safe_integers: false,
            })
            .expect("open real native database");
    }

    fn exec(worker: &mut Worker, id: u64, sql: &str) {
        worker
            .call(Command::Request(Request::Exec {
                id,
                sql: sql.to_owned(),
            }))
            .expect(sql);
    }

    fn wait_for_worker_exit(worker: &Worker) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while worker
            .thread
            .as_ref()
            .is_some_and(|thread| !thread.is_finished())
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            worker.thread.as_ref().is_some_and(JoinHandle::is_finished),
            "native engine did not acknowledge cancellation"
        );
    }

    #[test]
    fn real_lock_wait_has_its_own_allowance_and_busy_connections_remain_reusable() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = crate::extensions::safe_canonicalize(temp.path()).join("busy-budget.db");
        let mut worker = Worker::start().expect("worker");
        open_file(&mut worker, 1, &path);
        exec(
            &mut worker,
            1,
            "PRAGMA journal_mode = WAL; CREATE TABLE events(id INTEGER PRIMARY KEY)",
        );
        open_file(&mut worker, 2, &path);
        exec(
            &mut worker,
            1,
            "BEGIN IMMEDIATE; INSERT INTO events VALUES(99)",
        );

        for (setting, sql) in [
            // The first case reads the actual native setting. The other two
            // raise it within the same admitted batch, before the lock wait.
            ("PRAGMA busy_timeout=300", "BEGIN IMMEDIATE"),
            (
                "PRAGMA busy_timeout=0",
                "PRAGMA \"busy_timeout\"='300'; BEGIN IMMEDIATE",
            ),
            (
                "PRAGMA busy_timeout=0",
                "PRAGMA busy_timeout(300); BEGIN IMMEDIATE",
            ),
        ] {
            exec(&mut worker, 2, setting);
            let started = Instant::now();
            let error = worker
                .call_with_budget(
                    Command::Request(Request::Exec {
                        id: 2,
                        sql: sql.to_owned(),
                    }),
                    Duration::from_millis(100),
                    None,
                )
                .expect_err("competing writer must exhaust its native busy timeout");
            assert_eq!(error.primary_code, Some(5), "{error}");
            assert!(error.code.starts_with("SQLITE_BUSY"), "{error}");
            assert!(
                started.elapsed() >= Duration::from_millis(250),
                "native lock waiting must exceed the separate 100ms processing budget"
            );
            assert!(
                !worker.stopped.load(Ordering::Acquire),
                "normal BUSY must not poison the worker"
            );
            assert_eq!(
                worker
                    .call(Command::Request(Request::InTransaction { id: 2 }))
                    .unwrap(),
                false
            );
        }
        exec(&mut worker, 1, "ROLLBACK");
        exec(
            &mut worker,
            2,
            "BEGIN IMMEDIATE; INSERT INTO events VALUES(1); COMMIT",
        );
        worker.call(Command::Reset).expect("close native handles");
        open_file(&mut worker, 3, &path);
        let rows = worker
            .call(Command::Request(Request::Query {
                id: 3,
                sql: "SELECT id FROM events ORDER BY id".into(),
                bindings: Bindings::default(),
                mode: QueryMode::Values,
            }))
            .expect("reopened persistent state");
        assert_eq!(rows["rows"], json!([[1]]));
        worker
            .call(Command::Reset)
            .expect("close reopened database");
    }

    #[test]
    fn cancelling_a_real_lock_wait_cannot_commit_after_the_lock_is_released() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = crate::extensions::safe_canonicalize(temp.path()).join("cancelled-writer.db");
        let mut locker = Worker::start().expect("locker worker");
        let mut contender = Worker::start().expect("contender worker");
        open_file(&mut locker, 1, &path);
        exec(
            &mut locker,
            1,
            "PRAGMA journal_mode=WAL; CREATE TABLE events(id INTEGER PRIMARY KEY); INSERT INTO events VALUES(1)",
        );
        open_file(&mut contender, 1, &path);
        exec(&mut contender, 1, "PRAGMA busy_timeout=1000");
        exec(
            &mut locker,
            1,
            "BEGIN IMMEDIATE; INSERT INTO events VALUES(2)",
        );
        let started = Instant::now();
        let error = contender
            .call_with_timeout(
                Command::Request(Request::Exec {
                    id: 1,
                    sql: "INSERT INTO events VALUES(99)".into(),
                }),
                Duration::from_millis(100),
            )
            .expect_err("hard cancellation must dominate native wait allowance");
        assert_eq!(error.code, "SQLITE_INTERRUPT", "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "host cancellation must stay bounded"
        );
        assert!(contender.discard_on_stop.load(Ordering::Acquire));
        assert!(
            contender
                .call(Command::Request(Request::InTransaction { id: 1 }))
                .is_err()
        );
        // Release the real writer only after cancellation. A detached native
        // operation or teardown must not then insert the timed-out row.
        exec(&mut locker, 1, "ROLLBACK");
        wait_for_worker_exit(&contender);
        exec(&mut locker, 1, "INSERT INTO events VALUES(3)");
        locker
            .call(Command::Reset)
            .expect("close surviving database");
        open_file(&mut locker, 2, &path);
        let rows = locker
            .call(Command::Request(Request::Query {
                id: 2,
                sql: "SELECT id FROM events ORDER BY id".into(),
                bindings: Bindings::default(),
                mode: QueryMode::Values,
            }))
            .expect("durable rows after timed-out writer has exited");
        assert_eq!(rows["rows"], json!([[1], [3]]));
        locker.call(Command::Reset).expect("close verifier");
    }

    #[test]
    fn quarantined_runtime_discards_uncommitted_writes_and_cannot_reopen() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = crate::extensions::safe_canonicalize(temp.path());
        let path = root.join("quarantined.db");
        let mut worker = Worker::start().expect("worker");
        open_file(&mut worker, 1, &path);
        exec(
            &mut worker,
            1,
            "PRAGMA journal_mode=WAL; CREATE TABLE events(id INTEGER PRIMARY KEY); INSERT INTO events VALUES(1)",
        );
        exec(
            &mut worker,
            1,
            "BEGIN IMMEDIATE; INSERT INTO events VALUES(99)",
        );
        assert_eq!(
            worker
                .call(Command::Request(Request::InTransaction { id: 1 }))
                .expect("native transaction is live"),
            true
        );
        // Keep only the join handle so this test can observe real teardown
        // after the production retirement path has dropped its controller.
        let thread = worker.thread.take().expect("native worker thread");
        let mut runtime = SqliteRuntime {
            worker: Some(worker),
            databases: HashMap::from([(
                1,
                DatabaseOwner {
                    extension_id: Some("quarantined".into()),
                    path: Some(path.clone()),
                    readonly: false,
                },
            )]),
            next_id: 1,
            poisoned: false,
        };
        runtime.retire();
        runtime.retire(); // Repeated quarantine is idempotent.
        assert!(runtime.worker.is_none());
        assert!(runtime.databases.is_empty());
        assert!(runtime.reset().is_err());
        let modules = Rc::new(RefCell::new(PiJsModuleState::new()));
        let denied = runtime
            .dispatch(
                Request::Open {
                    path: ":memory:".into(),
                    readonly: false,
                    create: true,
                    strict: false,
                    safe_integers: false,
                },
                Some("quarantined".into()),
                &root,
                &modules,
                None,
            )
            .expect_err("a quarantined realm cannot start a replacement worker");
        assert_eq!(denied.code, "SQLITE_INTERRUPT");
        assert!(runtime.worker.is_none());

        let deadline = Instant::now() + Duration::from_secs(2);
        while !thread.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(thread.is_finished(), "quarantined native worker must exit");
        thread.join().expect("native cleanup did not panic");

        let mut verifier = Worker::start().expect("independent surviving worker");
        open_file(&mut verifier, 1, &path);
        exec(&mut verifier, 1, "INSERT INTO events VALUES(2)");
        let rows = verifier
            .call(Command::Request(Request::Query {
                id: 1,
                sql: "SELECT id FROM events ORDER BY id".into(),
                bindings: Bindings::default(),
                mode: QueryMode::Values,
            }))
            .expect("read reopened durable state");
        assert_eq!(rows["rows"], json!([[1], [2]]));
        verifier.call(Command::Reset).expect("close verifier");
    }

    #[test]
    fn sql_admission_decodes_identifiers_and_validates_every_statement() {
        for sql in [
            "SELECT \"load_extension\"('/tmp/no.so')",
            "SELECT 'readfile'('/tmp/private')",
            "SELECT `writefile`('/tmp/private', 'bad')",
            "SELECT [pragma_temp_store_directory]('/tmp')",
            "SELECT randomblob(2000000000)",
            "CREATE /* split */ TRIGGER bad AFTER INSERT ON t BEGIN SELECT 1; END",
            "CREATE VIRTUAL TABLE files USING \"csv\"(filename='/tmp/private')",
            "PRAGMA \"main\".\"temp_store_directory\" = '/tmp'",
            "INSERT INTO t VALUES('prefix'); /* tail */ ATTACH '/tmp/no.db' AS other",
            "INSERT INTO t VALUES('prefix'); CREATE TABLE missing_paren (",
        ] {
            assert!(admit_sql(sql, false).is_err(), "must reject {sql:?}");
        }
        assert!(
            admit_sql(
                "SELECT 'ATTACH; readfile(''only text'')' -- ; ATTACH ignored\n",
                true
            )
            .is_ok()
        );
        assert!(
            admit_sql(
                "CREATE VIRTUAL TABLE chunks USING fts5(body, tokenize='porter unicode61')",
                true
            )
            .is_ok()
        );
        assert!(
            admit_sql(
                "CREATE TABLE valid(id INTEGER PRIMARY KEY, payload BLOB)",
                true
            )
            .is_ok()
        );
        assert!(admit_sql("CREATE TABLE invalid(", true).is_err());
        assert!(admit_sql("SELECT 1; SELECT 2", true).is_err());
        assert_eq!(
            admit_sql("SELECT :same, ?1", true).unwrap().parameters,
            [Some(":same".to_owned())]
        );
    }

    #[test]
    fn result_budget_counts_json_escaping_and_raw_cell_sizes() {
        let mut bytes = MAX_RESULT_BYTES - 5;
        assert!(encode_value(&SqliteValue::Text("\0".into()), false, &mut bytes).is_err());
        let mut bytes = 0;
        assert!(
            encode_value(
                &SqliteValue::Text("x".repeat(MAX_VALUE_BYTES + 1).into()),
                false,
                &mut bytes
            )
            .is_err()
        );
    }

    #[test]
    fn actual_recursive_sql_is_cancelled_and_poisoned_worker_is_not_reused() {
        let mut worker = Worker::start().expect("worker");
        worker
            .call(Command::Open {
                id: 1,
                path: None,
                readonly: false,
                create: true,
                strict: false,
                safe_integers: false,
            })
            .expect("real memory database");
        exec(&mut worker, 1, "PRAGMA busy_timeout=0");
        let started = Instant::now();
        let error = worker.call_with_budget(Command::Request(Request::Query {
            id: 1,
            sql: "WITH RECURSIVE counter(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM counter) SELECT sum(n) FROM counter".into(),
            bindings: Bindings::default(),
            mode: QueryMode::Get,
        }), Duration::from_millis(100), None).expect_err("unbounded recursive query must time out");
        assert!(error.message.contains("SQLITE_INTERRUPT"), "{error}");
        assert_eq!(error.code, "SQLITE_INTERRUPT");
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(
            worker
                .call(Command::Request(Request::InTransaction { id: 1 }))
                .is_err()
        );
        // This is a real engine acknowledgment check, not merely a host
        // recv_timeout assertion. The worker must also release its connection.
        wait_for_worker_exit(&worker);
    }
}
