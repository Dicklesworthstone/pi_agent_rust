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
use fsqlite_parser::{Lexer, Parser, TokenKind};
use rquickjs::Ctx;
use rquickjs::function::Func;
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet};
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
    if !modules
        .borrow()
        .persistent_storage
        .allows(&path, owner, "write")
    {
        return Err("SQLite write access denied by the persistent storage grant".into());
    }
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

    fn normalized_sql<'a>(&self, sql: &'a str) -> std::borrow::Cow<'a, str> {
        let has_text_busy_timeout = self.statements.iter().any(|tokens| {
            tokens.first().and_then(Token::word) == Some("pragma")
                && tokens
                    .get(1)
                    .and_then(Token::name)
                    .is_some_and(|name| name.eq_ignore_ascii_case("busy_timeout"))
                && matches!(tokens.get(3), Some(Token::Text(_)))
        });
        if !has_text_busy_timeout {
            return std::borrow::Cow::Borrowed(sql);
        }
        let slices = split_sql_statement_slices(sql);
        if slices.len() != self.statements.len() {
            return std::borrow::Cow::Borrowed(sql);
        }
        let rewritten = slices
            .into_iter()
            .zip(&self.statements)
            .map(|(slice, tokens)| {
                if tokens.first().and_then(Token::word) == Some("pragma")
                    && tokens
                        .get(1)
                        .and_then(Token::name)
                        .is_some_and(|name| name.eq_ignore_ascii_case("busy_timeout"))
                {
                    if tokens.len() == 2 {
                        return "PRAGMA busy_timeout".to_string();
                    }
                    if let Some(ms) = tokens
                        .get(3)
                        .and_then(Token::name)
                        .and_then(|value| value.parse::<u64>().ok())
                    {
                        return format!("PRAGMA busy_timeout = {ms}");
                    }
                }
                slice.trim().to_string()
            })
            .collect::<Vec<_>>()
            .join("; ");
        std::borrow::Cow::Owned(rewritten)
    }
}

fn collect_from_table_names<'a>(from: &'a fsqlite_ast::FromClause, out: &mut Vec<&'a str>) {
    collect_source_table_names(&from.source, out);
    for join in &from.joins {
        collect_source_table_names(&join.table, out);
    }
}

fn collect_source_table_names<'a>(
    source: &'a fsqlite_ast::TableOrSubquery,
    out: &mut Vec<&'a str>,
) {
    match source {
        fsqlite_ast::TableOrSubquery::Table {
            name: qualified,
            alias,
            ..
        } => {
            if qualified.schema.is_none() {
                out.push(&qualified.name);
            }
            if let Some(alias) = alias.as_deref() {
                out.push(alias);
            }
        }
        fsqlite_ast::TableOrSubquery::ParenJoin(inner) => collect_from_table_names(inner, out),
        fsqlite_ast::TableOrSubquery::Subquery { .. }
        | fsqlite_ast::TableOrSubquery::TableFunction { .. } => {}
    }
}

fn expr_contains_fts5_aux_call(expr: &fsqlite_ast::Expr, table_names: &[&str]) -> bool {
    match expr {
        fsqlite_ast::Expr::FunctionCall {
            name,
            args,
            order_by,
            filter,
            over,
            ..
        } => {
            let is_fts5_aux = (name.eq_ignore_ascii_case("bm25")
                || name.eq_ignore_ascii_case("highlight")
                || name.eq_ignore_ascii_case("snippet"))
                && matches!(
                    args,
                    fsqlite_ast::FunctionArgs::List(list)
                        if matches!(
                            list.first(),
                            Some(fsqlite_ast::Expr::Column(col_ref, _))
                                if col_ref.schema.is_none()
                                    && col_ref.table.is_none()
                                    && table_names.iter().any(|t| col_ref.column.eq_ignore_ascii_case(t))
                        )
                );
            if is_fts5_aux {
                return true;
            }
            let in_args = match args {
                fsqlite_ast::FunctionArgs::Star => false,
                fsqlite_ast::FunctionArgs::List(list) => list
                    .iter()
                    .any(|arg| expr_contains_fts5_aux_call(arg, table_names)),
            };
            in_args
                || order_by
                    .iter()
                    .any(|term| expr_contains_fts5_aux_call(&term.expr, table_names))
                || filter
                    .as_deref()
                    .is_some_and(|f| expr_contains_fts5_aux_call(f, table_names))
                || over.as_ref().is_some_and(|spec| {
                    spec.partition_by
                        .iter()
                        .any(|e| expr_contains_fts5_aux_call(e, table_names))
                        || spec
                            .order_by
                            .iter()
                            .any(|t| expr_contains_fts5_aux_call(&t.expr, table_names))
                })
        }
        fsqlite_ast::Expr::BinaryOp { left, right, .. }
        | fsqlite_ast::Expr::JsonAccess {
            expr: left,
            path: right,
            ..
        } => {
            expr_contains_fts5_aux_call(left, table_names)
                || expr_contains_fts5_aux_call(right, table_names)
        }
        fsqlite_ast::Expr::UnaryOp { expr: inner, .. }
        | fsqlite_ast::Expr::Cast { expr: inner, .. }
        | fsqlite_ast::Expr::Collate { expr: inner, .. }
        | fsqlite_ast::Expr::IsNull { expr: inner, .. } => {
            expr_contains_fts5_aux_call(inner, table_names)
        }
        fsqlite_ast::Expr::Between {
            expr: target,
            low,
            high,
            ..
        } => {
            expr_contains_fts5_aux_call(target, table_names)
                || expr_contains_fts5_aux_call(low, table_names)
                || expr_contains_fts5_aux_call(high, table_names)
        }
        fsqlite_ast::Expr::In {
            expr: target, set, ..
        } => {
            expr_contains_fts5_aux_call(target, table_names)
                || matches!(
                    set,
                    fsqlite_ast::InSet::List(list)
                        if list.iter().any(|e| expr_contains_fts5_aux_call(e, table_names))
                )
        }
        fsqlite_ast::Expr::Like {
            expr: target,
            pattern,
            escape,
            ..
        } => {
            expr_contains_fts5_aux_call(target, table_names)
                || expr_contains_fts5_aux_call(pattern, table_names)
                || escape
                    .as_deref()
                    .is_some_and(|e| expr_contains_fts5_aux_call(e, table_names))
        }
        fsqlite_ast::Expr::Case {
            operand,
            whens,
            else_expr,
            ..
        } => {
            operand
                .as_deref()
                .is_some_and(|e| expr_contains_fts5_aux_call(e, table_names))
                || whens.iter().any(|(w, t)| {
                    expr_contains_fts5_aux_call(w, table_names)
                        || expr_contains_fts5_aux_call(t, table_names)
                })
                || else_expr
                    .as_deref()
                    .is_some_and(|e| expr_contains_fts5_aux_call(e, table_names))
        }
        fsqlite_ast::Expr::RowValue(items, _) => items
            .iter()
            .any(|e| expr_contains_fts5_aux_call(e, table_names)),
        fsqlite_ast::Expr::Literal(_, _)
        | fsqlite_ast::Expr::BoundOuterValue { .. }
        | fsqlite_ast::Expr::Column(_, _)
        | fsqlite_ast::Expr::Exists { .. }
        | fsqlite_ast::Expr::Subquery(_, _)
        | fsqlite_ast::Expr::Raise { .. }
        | fsqlite_ast::Expr::Placeholder(_, _) => false,
    }
}

fn visit_window_spec_placeholders_mut(
    spec: &mut fsqlite_ast::WindowSpec,
    visit: &mut impl FnMut(&mut fsqlite_ast::PlaceholderType, fsqlite_ast::Span),
) {
    for expr in &mut spec.partition_by {
        visit_expr_placeholders_mut(expr, visit);
    }
    for term in &mut spec.order_by {
        visit_expr_placeholders_mut(&mut term.expr, visit);
    }
    if let Some(frame) = &mut spec.frame {
        if let fsqlite_ast::FrameBound::Preceding(expr) | fsqlite_ast::FrameBound::Following(expr) =
            &mut frame.start
        {
            visit_expr_placeholders_mut(expr, visit);
        }
        if let Some(
            fsqlite_ast::FrameBound::Preceding(expr) | fsqlite_ast::FrameBound::Following(expr),
        ) = &mut frame.end
        {
            visit_expr_placeholders_mut(expr, visit);
        }
    }
}

fn visit_expr_placeholders_mut(
    expr: &mut fsqlite_ast::Expr,
    visit: &mut impl FnMut(&mut fsqlite_ast::PlaceholderType, fsqlite_ast::Span),
) {
    match expr {
        fsqlite_ast::Expr::Placeholder(kind, span) => visit(kind, *span),
        fsqlite_ast::Expr::BinaryOp { left, right, .. }
        | fsqlite_ast::Expr::JsonAccess {
            expr: left,
            path: right,
            ..
        } => {
            visit_expr_placeholders_mut(left, visit);
            visit_expr_placeholders_mut(right, visit);
        }
        fsqlite_ast::Expr::UnaryOp { expr: inner, .. }
        | fsqlite_ast::Expr::Cast { expr: inner, .. }
        | fsqlite_ast::Expr::Collate { expr: inner, .. }
        | fsqlite_ast::Expr::IsNull { expr: inner, .. } => {
            visit_expr_placeholders_mut(inner, visit);
        }
        fsqlite_ast::Expr::Between {
            expr: target,
            low,
            high,
            ..
        } => {
            visit_expr_placeholders_mut(target, visit);
            visit_expr_placeholders_mut(low, visit);
            visit_expr_placeholders_mut(high, visit);
        }
        fsqlite_ast::Expr::In {
            expr: target, set, ..
        } => {
            visit_expr_placeholders_mut(target, visit);
            match set {
                fsqlite_ast::InSet::List(list) => {
                    for item in list {
                        visit_expr_placeholders_mut(item, visit);
                    }
                }
                fsqlite_ast::InSet::Subquery(subquery) => {
                    visit_select_placeholders_mut(subquery, visit);
                }
                fsqlite_ast::InSet::Table(_) => {}
            }
        }
        fsqlite_ast::Expr::Like {
            expr: target,
            pattern,
            escape,
            ..
        } => {
            visit_expr_placeholders_mut(target, visit);
            visit_expr_placeholders_mut(pattern, visit);
            if let Some(esc) = escape {
                visit_expr_placeholders_mut(esc, visit);
            }
        }
        fsqlite_ast::Expr::Case {
            operand,
            whens,
            else_expr,
            ..
        } => {
            if let Some(op) = operand {
                visit_expr_placeholders_mut(op, visit);
            }
            for (when_expr, then_expr) in whens {
                visit_expr_placeholders_mut(when_expr, visit);
                visit_expr_placeholders_mut(then_expr, visit);
            }
            if let Some(el) = else_expr {
                visit_expr_placeholders_mut(el, visit);
            }
        }
        fsqlite_ast::Expr::Exists { subquery, .. } | fsqlite_ast::Expr::Subquery(subquery, _) => {
            visit_select_placeholders_mut(subquery, visit);
        }
        fsqlite_ast::Expr::FunctionCall {
            args,
            order_by,
            filter,
            over,
            ..
        } => {
            if let fsqlite_ast::FunctionArgs::List(list) = args {
                for arg in list {
                    visit_expr_placeholders_mut(arg, visit);
                }
            }
            for term in order_by {
                visit_expr_placeholders_mut(&mut term.expr, visit);
            }
            if let Some(f) = filter {
                visit_expr_placeholders_mut(f, visit);
            }
            if let Some(spec) = over {
                visit_window_spec_placeholders_mut(spec, visit);
            }
        }
        fsqlite_ast::Expr::RowValue(items, _) => {
            for item in items {
                visit_expr_placeholders_mut(item, visit);
            }
        }
        fsqlite_ast::Expr::Literal(_, _)
        | fsqlite_ast::Expr::BoundOuterValue { .. }
        | fsqlite_ast::Expr::Column(_, _)
        | fsqlite_ast::Expr::Raise { .. } => {}
    }
}

fn visit_from_placeholders_mut(
    from: &mut fsqlite_ast::FromClause,
    visit: &mut impl FnMut(&mut fsqlite_ast::PlaceholderType, fsqlite_ast::Span),
) {
    visit_table_source_placeholders_mut(&mut from.source, visit);
    for join in &mut from.joins {
        visit_table_source_placeholders_mut(&mut join.table, visit);
        if let Some(fsqlite_ast::JoinConstraint::On(expr)) = &mut join.constraint {
            visit_expr_placeholders_mut(expr, visit);
        }
    }
}

fn visit_table_source_placeholders_mut(
    source: &mut fsqlite_ast::TableOrSubquery,
    visit: &mut impl FnMut(&mut fsqlite_ast::PlaceholderType, fsqlite_ast::Span),
) {
    match source {
        fsqlite_ast::TableOrSubquery::Table { .. } => {}
        fsqlite_ast::TableOrSubquery::Subquery { query, .. } => {
            visit_select_placeholders_mut(query, visit);
        }
        fsqlite_ast::TableOrSubquery::TableFunction { args, .. } => {
            for arg in args {
                visit_expr_placeholders_mut(arg, visit);
            }
        }
        fsqlite_ast::TableOrSubquery::ParenJoin(inner) => {
            visit_from_placeholders_mut(inner, visit);
        }
    }
}

fn visit_select_core_placeholders_mut(
    core: &mut fsqlite_ast::SelectCore,
    visit: &mut impl FnMut(&mut fsqlite_ast::PlaceholderType, fsqlite_ast::Span),
) {
    match core {
        fsqlite_ast::SelectCore::Select {
            columns,
            from,
            where_clause,
            group_by,
            having,
            windows,
            ..
        } => {
            for col in columns {
                if let fsqlite_ast::ResultColumn::Expr { expr, .. } = col {
                    visit_expr_placeholders_mut(expr, visit);
                }
            }
            if let Some(from_clause) = from {
                visit_from_placeholders_mut(from_clause, visit);
            }
            if let Some(where_expr) = where_clause {
                visit_expr_placeholders_mut(where_expr, visit);
            }
            for expr in group_by {
                visit_expr_placeholders_mut(expr, visit);
            }
            if let Some(having_expr) = having {
                visit_expr_placeholders_mut(having_expr, visit);
            }
            for win in windows {
                visit_window_spec_placeholders_mut(&mut win.spec, visit);
            }
        }
        fsqlite_ast::SelectCore::Values(values) => {
            for row in values.iter_mut() {
                for expr in row {
                    visit_expr_placeholders_mut(expr, visit);
                }
            }
        }
    }
}

fn visit_select_placeholders_mut(
    select: &mut fsqlite_ast::SelectStatement,
    visit: &mut impl FnMut(&mut fsqlite_ast::PlaceholderType, fsqlite_ast::Span),
) {
    if let Some(with) = &mut select.with {
        for cte in &mut with.ctes {
            visit_select_placeholders_mut(&mut cte.query, visit);
        }
    }
    visit_select_core_placeholders_mut(&mut select.body.select, visit);
    for (_, core) in &mut select.body.compounds {
        visit_select_core_placeholders_mut(core, visit);
    }
    for term in &mut select.order_by {
        visit_expr_placeholders_mut(&mut term.expr, visit);
    }
    if let Some(limit) = &mut select.limit {
        visit_expr_placeholders_mut(&mut limit.limit, visit);
        if let Some(offset) = &mut limit.offset {
            visit_expr_placeholders_mut(offset, visit);
        }
    }
}

/// Work around `fsqlite-core 0.4.10` rejecting unprojected FTS5 auxiliary
/// functions (`bm25`, `highlight`, `snippet`) in `ORDER BY` by projecting them
/// as hidden trailing `SELECT` columns and truncating them from query output.
fn rewrite_unprojected_fts5_order_by_select(
    stmt: Option<&SqlStatement>,
) -> (Option<String>, usize) {
    let Some(SqlStatement::Select(select)) = stmt else {
        return (None, 0);
    };
    if !select.body.compounds.is_empty() || select.order_by.is_empty() {
        return (None, 0);
    }
    let fsqlite_ast::SelectCore::Select {
        distinct: fsqlite_ast::Distinctness::All,
        columns: select_cols,
        from: Some(from),
        ..
    } = &select.body.select
    else {
        return (None, 0);
    };

    let mut table_names = Vec::new();
    collect_from_table_names(from, &mut table_names);
    if table_names.is_empty() {
        return (None, 0);
    }

    let mut needs_hidden = false;
    for term in &select.order_by {
        let already_projected = select_cols.iter().any(|col| {
            matches!(
                col,
                fsqlite_ast::ResultColumn::Expr { expr, .. } if expr == &term.expr
            )
        });
        if !already_projected && expr_contains_fts5_aux_call(&term.expr, &table_names) {
            needs_hidden = true;
            break;
        }
    }
    if !needs_hidden {
        return (None, 0);
    }

    let mut rewritten = select.clone();
    let mut slots = Vec::new();
    visit_select_placeholders_mut(&mut rewritten, &mut |kind, span| {
        slots.push((span.start, kind.clone()));
    });
    if slots
        .iter()
        .any(|(_, kind)| matches!(kind, fsqlite_ast::PlaceholderType::Anonymous))
    {
        slots.sort_by_key(|(start, _)| *start);
        let mut max_ordinal = 0_u32;
        let mut named = HashMap::<(char, String), u32>::new();
        let mut anon_ordinals = HashMap::<u32, u32>::new();
        for (start, kind) in slots {
            let ordinal = match kind {
                fsqlite_ast::PlaceholderType::Anonymous => {
                    let next = max_ordinal.saturating_add(1);
                    anon_ordinals.insert(start, next);
                    next
                }
                fsqlite_ast::PlaceholderType::Numbered(n) => n,
                fsqlite_ast::PlaceholderType::ColonNamed(ident) => *named
                    .entry((':', ident))
                    .or_insert_with(|| max_ordinal.saturating_add(1)),
                fsqlite_ast::PlaceholderType::AtNamed(ident) => *named
                    .entry(('@', ident))
                    .or_insert_with(|| max_ordinal.saturating_add(1)),
                fsqlite_ast::PlaceholderType::DollarNamed(ident) => *named
                    .entry(('$', ident))
                    .or_insert_with(|| max_ordinal.saturating_add(1)),
            };
            max_ordinal = max_ordinal.max(ordinal);
        }
        visit_select_placeholders_mut(&mut rewritten, &mut |kind, span| {
            if matches!(kind, fsqlite_ast::PlaceholderType::Anonymous)
                && let Some(&ordinal) = anon_ordinals.get(&span.start)
            {
                *kind = fsqlite_ast::PlaceholderType::Numbered(ordinal);
            }
        });
    }

    let order_terms = rewritten.order_by.clone();
    let fsqlite_ast::SelectCore::Select {
        columns: rewritten_cols,
        ..
    } = &mut rewritten.body.select
    else {
        return (None, 0);
    };
    let mut hidden_col_count = 0_usize;
    for term in order_terms {
        let already_projected = rewritten_cols.iter().any(|col| {
            matches!(
                col,
                fsqlite_ast::ResultColumn::Expr { expr, .. } if expr == &term.expr
            )
        });
        if !already_projected && expr_contains_fts5_aux_call(&term.expr, &table_names) {
            rewritten_cols.push(fsqlite_ast::ResultColumn::Expr {
                expr: term.expr,
                alias: None,
            });
            hidden_col_count += 1;
        }
    }
    if hidden_col_count == 0 {
        (None, 0)
    } else {
        (Some(rewritten.to_string()), hidden_col_count)
    }
}

fn canonicalize_sql_parameter_tokens(sql: &str) -> Result<String> {
    let mut chars = sql.chars().peekable();
    let mut out = String::with_capacity(sql.len() + 8);
    let mut max_ordinal = 0_usize;
    let mut named = HashMap::<String, usize>::new();
    while let Some(ch) = chars.next() {
        if ch == '-' && chars.peek() == Some(&'-') {
            out.push(ch);
            if let Some(second) = chars.next() {
                out.push(second);
            }
            for comment_ch in chars.by_ref() {
                out.push(comment_ch);
                if comment_ch == '\n' || comment_ch == '\r' {
                    break;
                }
            }
            continue;
        }
        if ch == '/' && chars.peek() == Some(&'*') {
            out.push(ch);
            if let Some(second) = chars.next() {
                out.push(second);
            }
            while let Some(comment_ch) = chars.next() {
                out.push(comment_ch);
                if comment_ch == '*' && chars.peek() == Some(&'/') {
                    if let Some(end_slash) = chars.next() {
                        out.push(end_slash);
                    }
                    break;
                }
            }
            continue;
        }
        if matches!(ch, '\'' | '"' | '`' | '[') {
            let close = if ch == '[' { ']' } else { ch };
            out.push(ch);
            while let Some(next) = chars.next() {
                out.push(next);
                if next == close {
                    if close != ']' && chars.peek() == Some(&close) {
                        if let Some(escaped) = chars.next() {
                            out.push(escaped);
                        }
                    } else {
                        break;
                    }
                }
            }
            continue;
        }
        if matches!(ch, '?' | ':' | '@' | '$') {
            let mut name = ch.to_string();
            while let Some(&next) = chars.peek() {
                if (ch == '?' && next.is_ascii_digit())
                    || (ch != '?' && (next.is_alphanumeric() || next == '_'))
                {
                    name.push(next);
                    chars.next();
                } else {
                    break;
                }
            }
            let ordinal = if name == "?" {
                max_ordinal + 1
            } else if let Some(index) = name.strip_prefix('?') {
                index
                    .parse::<usize>()
                    .map_err(|_| "Invalid numbered SQL parameter")?
            } else if let Some(&index) = named.get(&name) {
                index
            } else {
                let index = max_ordinal + 1;
                named.insert(name, index);
                index
            };
            if ordinal == 0 || ordinal > MAX_PARAMETERS {
                return Err("SQL parameter limit exceeded (1 through 999)".into());
            }
            max_ordinal = max_ordinal.max(ordinal);
            out.push('?');
            out.push_str(&ordinal.to_string());
            continue;
        }
        if ch.is_alphanumeric() || ch == '_' {
            out.push(ch);
            while let Some(&next) = chars.peek() {
                if next.is_alphanumeric() || next == '_' || next == '$' {
                    out.push(next);
                    chars.next();
                } else {
                    break;
                }
            }
            continue;
        }
        out.push(ch);
    }
    Ok(out)
}

fn dqs_double_quoted_identifier_tokens(sql: &str) -> Vec<(u32, u32, String)> {
    Lexer::tokenize(sql)
        .into_iter()
        .filter_map(|tok| match tok.kind {
            TokenKind::QuotedId(name, true) => {
                Some((tok.span.start, tok.span.end, name.to_string()))
            }
            _ => None,
        })
        .collect()
}

fn dqs_missing_column_name(err: &FrankenError) -> Option<String> {
    let msg = match err {
        FrankenError::Internal(m)
        | FrankenError::NotImplemented(m)
        | FrankenError::FunctionError(m) => m.as_str(),
        _ => return None,
    };
    if let Some((_, rest)) = msg.split_once("no such column: ") {
        let name = rest
            .split_once(" in table ")
            .map_or(rest, |(head, _)| head)
            .trim();
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }
    if let Some(name) = msg
        .strip_prefix("column not found: ")
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        return Some(name.to_string());
    }
    if let Some((_, rest)) = msg.split_once("column: \"")
        && let Some((name, _)) = rest.split_once('"')
        && !name.is_empty()
    {
        return Some(name.to_string());
    }
    None
}

fn dqs_rewrite_once(
    sql: &str,
    err: &FrankenError,
    rewritten: &mut HashSet<String>,
) -> Option<String> {
    let name = dqs_missing_column_name(err)?;
    if !rewritten.insert(name.clone()) {
        return None;
    }
    let mut ranges: Vec<(u32, u32)> = dqs_double_quoted_identifier_tokens(sql)
        .into_iter()
        .filter(|(_, _, candidate)| candidate == &name)
        .map(|(start, end, _)| (start, end))
        .collect();
    if ranges.is_empty() {
        return None;
    }
    ranges.sort_by_key(|&(start, _)| std::cmp::Reverse(start));
    let literal = format!("'{}'", name.replace('\'', "''"));
    let mut out = sql.to_string();
    for (start, end) in ranges {
        let (Ok(start), Ok(end)) = (usize::try_from(start), usize::try_from(end)) else {
            return None;
        };
        out.replace_range(start..end, &literal);
    }
    Some(out)
}

fn dqs_insert_values_rewrite(sql: &str) -> Option<String> {
    let (stmts, errors) = Parser::from_sql(sql).parse_all();
    if !errors.is_empty() || stmts.len() != 1 {
        return None;
    }
    let SqlStatement::Insert(insert) = &stmts[0] else {
        return None;
    };
    let fsqlite_ast::InsertSource::Values(rows) = &insert.source else {
        return None;
    };
    let dq_tokens = dqs_double_quoted_identifier_tokens(sql);
    if dq_tokens.is_empty() {
        return None;
    }
    let mut ranges: Vec<(u32, u32, String)> = Vec::new();
    for row in rows {
        for expr in row {
            if let fsqlite_ast::Expr::Column(col_ref, span) = expr
                && col_ref.table.is_none()
                && dq_tokens.iter().any(|(start, end, name)| {
                    *start == span.start
                        && *end == span.end
                        && name.as_str() == col_ref.column.as_ref()
                })
            {
                ranges.push((span.start, span.end, col_ref.column.to_string()));
            }
        }
    }
    if ranges.is_empty() {
        return None;
    }
    ranges.sort_by_key(|(start, _, _)| std::cmp::Reverse(*start));
    let mut out = sql.to_string();
    for (start, end, name) in ranges {
        let (Ok(start), Ok(end)) = (usize::try_from(start), usize::try_from(end)) else {
            return None;
        };
        let literal = format!("'{}'", name.replace('\'', "''"));
        out.replace_range(start..end, &literal);
    }
    Some(out)
}

async fn prepare_with_dqs<'conn, 'sql>(
    conn: &'conn Connection,
    sql: &'sql str,
) -> NativeResult<(
    fsqlite::PreparedStatement<'conn>,
    std::borrow::Cow<'sql, str>,
)> {
    if !sql.contains('"') {
        let stmt = conn.prepare(sql).await.map_err(SqliteFailure::from)?;
        return Ok((stmt, std::borrow::Cow::Borrowed(sql)));
    }
    let candidate = dqs_insert_values_rewrite(sql)
        .map_or(std::borrow::Cow::Borrowed(sql), std::borrow::Cow::Owned);
    match conn.prepare(candidate.as_ref()).await {
        Ok(stmt) => Ok((stmt, candidate)),
        Err(first) => {
            let mut rewritten = HashSet::new();
            let Some(mut cur) = dqs_rewrite_once(candidate.as_ref(), &first, &mut rewritten) else {
                return Err(SqliteFailure::from(first));
            };
            loop {
                match conn.prepare(&cur).await {
                    Ok(stmt) => return Ok((stmt, std::borrow::Cow::Owned(cur))),
                    Err(err) => {
                        let Some(next) = dqs_rewrite_once(&cur, &err, &mut rewritten) else {
                            return Err(SqliteFailure::from(err));
                        };
                        cur = next;
                    }
                }
            }
        }
    }
}

fn split_sql_statement_slices(sql: &str) -> Vec<&str> {
    let mut slices = Vec::new();
    let bytes = sql.as_bytes();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' | b'`' => {
                let quote = bytes[i];
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == quote {
                        i += 1;
                        if i < bytes.len() && bytes[i] == quote {
                            i += 1;
                        } else {
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
            }
            b'[' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b']' {
                    i += 1;
                }
                if i < bytes.len() {
                    i += 1;
                }
            }
            b'-' if i + 1 < bytes.len() && bytes[i + 1] == b'-' => {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' && bytes[i] != b'\r' {
                    i += 1;
                }
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
            }
            b';' => {
                let slice = sql[start..i].trim();
                if !slice.is_empty() {
                    slices.push(slice);
                }
                i += 1;
                start = i;
            }
            _ => {
                i += 1;
            }
        }
    }
    let tail = sql[start..].trim();
    if !tail.is_empty() {
        slices.push(tail);
    }
    slices
}

async fn dml_returning_column_names(
    conn: &Connection,
    stmt: Option<&SqlStatement>,
) -> NativeResult<Vec<String>> {
    let (table_name, returning) = match stmt {
        Some(SqlStatement::Insert(ins)) => (ins.table.name.as_str(), ins.returning.as_slice()),
        Some(SqlStatement::Update(upd)) => (upd.table.name.name.as_str(), upd.returning.as_slice()),
        Some(SqlStatement::Delete(del)) => (del.table.name.name.as_str(), del.returning.as_slice()),
        _ => return Ok(Vec::new()),
    };
    if returning.is_empty() {
        return Ok(Vec::new());
    }
    let needs_star = returning.iter().any(|col| {
        matches!(
            col,
            fsqlite_ast::ResultColumn::Star | fsqlite_ast::ResultColumn::TableStar(_)
        )
    });
    let table_cols = if needs_star {
        let escaped = table_name.replace('"', "\"\"");
        let rows = conn
            .query(&format!("PRAGMA table_info(\"{escaped}\")"))
            .await
            .map_err(SqliteFailure::from)?;
        rows.into_iter()
            .filter_map(|row| match row.get(1) {
                Some(SqliteValue::Text(name)) => Some(name.to_string()),
                _ => None,
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let mut cols = Vec::with_capacity(returning.len().max(table_cols.len()));
    for (idx, col) in returning.iter().enumerate() {
        match col {
            fsqlite_ast::ResultColumn::Star | fsqlite_ast::ResultColumn::TableStar(_) => {
                cols.extend(table_cols.iter().cloned());
            }
            fsqlite_ast::ResultColumn::Expr {
                alias: Some(alias), ..
            } => {
                cols.push(alias.clone());
            }
            fsqlite_ast::ResultColumn::Expr {
                expr: fsqlite_ast::Expr::Column(col_ref, _),
                alias: None,
            } => {
                cols.push(col_ref.column.to_string());
            }
            fsqlite_ast::ResultColumn::Expr { alias: None, .. } => {
                cols.push(format!("column{}", idx + 1));
            }
        }
    }
    Ok(cols)
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

#[cfg(unix)]
struct ExternalWalWriteLease {
    file: fsqlite::fsqlite_vfs::UnixFile,
}

#[cfg(unix)]
impl ExternalWalWriteLease {
    fn try_acquire(
        path: &Path,
        cx: &fsqlite_types::cx::Cx,
    ) -> std::result::Result<Self, FrankenError> {
        use fsqlite::fsqlite_vfs::{UnixVfs, Vfs as _, VfsFile as _};
        use fsqlite_types::flags::VfsOpenFlags;

        let vfs = UnixVfs::new();
        let (mut file, _) = vfs.open(
            cx,
            Some(path),
            VfsOpenFlags::MAIN_DB | VfsOpenFlags::READWRITE,
        )?;
        if let Err(err) = file.compat_writer_hold_wal_write_lock(cx) {
            let uncancelled = fsqlite_types::cx::Cx::new();
            let _ = file.close(&uncancelled);
            return Err(err);
        }
        Ok(Self { file })
    }

    fn release(mut self) -> std::result::Result<(), FrankenError> {
        use fsqlite::fsqlite_vfs::VfsFile as _;

        let uncancelled = fsqlite_types::cx::Cx::new();
        let release_err = self
            .file
            .compat_writer_release_wal_write_lock(&uncancelled)
            .err();
        let close_err = self.file.close(&uncancelled).err();
        release_err.or(close_err).map_or(Ok(()), Err)
    }
}

#[cfg(windows)]
struct ExternalWalWriteLease {
    file: fsqlite::fsqlite_vfs::WindowsFile,
}

#[cfg(windows)]
impl ExternalWalWriteLease {
    fn try_acquire(
        path: &Path,
        cx: &fsqlite_types::cx::Cx,
    ) -> std::result::Result<Self, FrankenError> {
        use fsqlite::fsqlite_vfs::shm::{SQLITE_SHM_EXCLUSIVE, SQLITE_SHM_LOCK, WAL_WRITE_LOCK};
        use fsqlite::fsqlite_vfs::{Vfs as _, VfsFile as _, WindowsVfs};
        use fsqlite_types::LockLevel;
        use fsqlite_types::flags::VfsOpenFlags;

        let vfs = WindowsVfs::new();
        let (mut file, _) = vfs.open(
            cx,
            Some(path),
            VfsOpenFlags::MAIN_DB | VfsOpenFlags::READWRITE,
        )?;
        if let Err(err) = file.lock(cx, LockLevel::Shared) {
            let uncancelled = fsqlite_types::cx::Cx::new();
            let _ = file.close(&uncancelled);
            return Err(err);
        }
        if let Err(err) = file.shm_lock(
            cx,
            WAL_WRITE_LOCK,
            1,
            SQLITE_SHM_LOCK | SQLITE_SHM_EXCLUSIVE,
        ) {
            let uncancelled = fsqlite_types::cx::Cx::new();
            let _ = file.unlock(&uncancelled, LockLevel::None);
            let _ = file.close(&uncancelled);
            return Err(err);
        }
        Ok(Self { file })
    }

    fn release(mut self) -> std::result::Result<(), FrankenError> {
        use fsqlite::fsqlite_vfs::VfsFile as _;
        use fsqlite::fsqlite_vfs::shm::{SQLITE_SHM_EXCLUSIVE, SQLITE_SHM_UNLOCK, WAL_WRITE_LOCK};
        use fsqlite_types::LockLevel;

        let uncancelled = fsqlite_types::cx::Cx::new();
        let shm_err = self
            .file
            .shm_lock(
                &uncancelled,
                WAL_WRITE_LOCK,
                1,
                SQLITE_SHM_UNLOCK | SQLITE_SHM_EXCLUSIVE,
            )
            .err();
        let unlock_err = self.file.unlock(&uncancelled, LockLevel::None).err();
        let close_err = self.file.close(&uncancelled).err();
        shm_err.or(unlock_err).or(close_err).map_or(Ok(()), Err)
    }
}

#[cfg(not(any(unix, windows)))]
struct ExternalWalWriteLease;

#[cfg(not(any(unix, windows)))]
impl ExternalWalWriteLease {
    fn try_acquire(
        _path: &Path,
        _cx: &fsqlite_types::cx::Cx,
    ) -> std::result::Result<Self, FrankenError> {
        Ok(Self)
    }

    #[allow(clippy::unnecessary_wraps)]
    fn release(self) -> std::result::Result<(), FrankenError> {
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct SchemaFailProfile {
    auto_ipk_col_lower: Option<String>,
}

struct EngineDatabase {
    // Drop/release the external WAL write lease before closing the engine, and
    // drop/close the engine before releasing its directory guards.
    wal_write_lease: RefCell<Option<ExternalWalWriteLease>>,
    conn: Connection,
    parent: Option<storage::StorageParent>,
    readonly: bool,
    strict: bool,
    safe_integers: bool,
    txn_has_writes: Cell<bool>,
    schema_version: Cell<Option<i64>>,
    schema_fail_tables: RefCell<HashMap<String, SchemaFailProfile>>,
    has_temp_schema: Cell<bool>,
    namespace_valid: Cell<bool>,
    poisoned: Cell<bool>,
}

impl EngineDatabase {
    fn writable_db_path(&self) -> Option<&Path> {
        (!self.readonly)
            .then(|| self.parent.as_ref().map(storage::StorageParent::path))
            .flatten()
    }

    fn release_wal_write_lease(&self) -> NativeResult<()> {
        if let Some(lease) = self.wal_write_lease.borrow_mut().take() {
            lease.release().map_err(SqliteFailure::from)?;
        }
        Ok(())
    }

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
                wal_write_lease: RefCell::new(None),
                conn,
                parent,
                readonly,
                strict,
                safe_integers,
                txn_has_writes: Cell::new(false),
                schema_version: Cell::new(None),
                schema_fail_tables: RefCell::new(HashMap::new()),
                has_temp_schema: Cell::new(false),
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
            database.txn_has_writes.set(false);
            database.release_wal_write_lease()?;
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
                execute_request(database, request, budget, &operation_cx).await
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
                database.txn_has_writes.set(false);
                if let Err(error) = database.release_wal_write_lease() {
                    first_error.get_or_insert(error);
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
    let mut rows = database
        .conn
        .query("SELECT type, tbl_name, sql FROM sqlite_master WHERE sql IS NOT NULL LIMIT 1025")
        .await
        .map_err(SqliteFailure::from)?;
    if rows.len() > 1024 {
        return Err("SQLite schema exceeds 1024 objects".into());
    }
    if database.has_temp_schema.get()
        && let Ok(temp_rows) = database
            .conn
            .query(
                "SELECT type, tbl_name, sql FROM sqlite_temp_master WHERE sql IS NOT NULL LIMIT 1025",
            )
            .await
    {
        rows.extend(temp_rows);
    }
    let mut fail_tables = HashMap::new();
    let mut tables_with_aux_ddl = HashSet::new();
    for row in rows {
        let Some(SqliteValue::Text(kind)) = row.get(0) else {
            return Err("Invalid SQLite schema object type".into());
        };
        if kind.as_ref() == "trigger" {
            return Err("Existing SQLite triggers are not permitted in extension databases".into());
        }
        let Some(SqliteValue::Text(sql)) = row.get(2) else {
            return Err("Invalid SQLite schema text".into());
        };
        let admitted = admit_sql(sql.as_ref(), true)?;
        if let Some(SqliteValue::Text(tbl_name)) = row.get(1) {
            let key = tbl_name.as_ref().to_ascii_lowercase();
            if kind.as_ref() == "table" {
                if let Some(SqlStatement::CreateTable(create)) = admitted.parsed.first()
                    && let Some(profile) = schema_fail_profile(create)
                {
                    fail_tables.insert(key, profile);
                } else {
                    fail_tables.remove(&key);
                }
            } else {
                tables_with_aux_ddl.insert(key);
            }
        }
    }
    for tbl in tables_with_aux_ddl {
        fail_tables.remove(&tbl);
    }
    *database.schema_fail_tables.borrow_mut() = fail_tables;
    database.schema_version.set(Some(version));
    Ok(())
}

fn schema_fail_profile(create: &fsqlite_ast::CreateTableStatement) -> Option<SchemaFailProfile> {
    let fsqlite_ast::CreateTableBody::Columns {
        columns,
        constraints,
    } = &create.body
    else {
        return None;
    };
    let mut has_fail = false;
    let mut has_non_fail = false;
    let mut saw_pk = false;
    let mut auto_ipk_col_lower = None;
    for col in columns {
        let is_integer = col
            .type_name
            .as_ref()
            .is_some_and(|ty| ty.name.eq_ignore_ascii_case("integer"));
        for constraint in &col.constraints {
            match &constraint.kind {
                fsqlite_ast::ColumnConstraintKind::PrimaryKey { conflict, .. } => {
                    if saw_pk {
                        has_non_fail = true;
                    } else {
                        saw_pk = true;
                        match conflict {
                            Some(fsqlite_ast::ConflictAction::Fail) => has_fail = true,
                            None if !create.without_rowid && is_integer => {
                                auto_ipk_col_lower = Some(col.name.to_ascii_lowercase());
                            }
                            _ => has_non_fail = true,
                        }
                    }
                }
                fsqlite_ast::ColumnConstraintKind::Unique { conflict }
                | fsqlite_ast::ColumnConstraintKind::NotNull { conflict } => {
                    if *conflict == Some(fsqlite_ast::ConflictAction::Fail) {
                        has_fail = true;
                    } else {
                        has_non_fail = true;
                    }
                }
                fsqlite_ast::ColumnConstraintKind::Check(_)
                | fsqlite_ast::ColumnConstraintKind::ForeignKey(_) => {
                    has_non_fail = true;
                }
                fsqlite_ast::ColumnConstraintKind::Null
                | fsqlite_ast::ColumnConstraintKind::Default(_)
                | fsqlite_ast::ColumnConstraintKind::Collate(_)
                | fsqlite_ast::ColumnConstraintKind::Generated { .. } => {}
            }
        }
    }
    for constraint in constraints {
        match &constraint.kind {
            fsqlite_ast::TableConstraintKind::PrimaryKey { conflict, .. } => {
                if saw_pk {
                    has_non_fail = true;
                } else {
                    saw_pk = true;
                    auto_ipk_col_lower = None;
                    if *conflict == Some(fsqlite_ast::ConflictAction::Fail) {
                        has_fail = true;
                    } else {
                        has_non_fail = true;
                    }
                }
            }
            fsqlite_ast::TableConstraintKind::Unique { conflict, .. } => {
                if *conflict == Some(fsqlite_ast::ConflictAction::Fail) {
                    has_fail = true;
                } else {
                    has_non_fail = true;
                }
            }
            fsqlite_ast::TableConstraintKind::Check(_)
            | fsqlite_ast::TableConstraintKind::ForeignKey { .. } => {
                has_non_fail = true;
            }
        }
    }
    if create.strict {
        has_non_fail = true;
    }
    (has_fail && !has_non_fail).then_some(SchemaFailProfile { auto_ipk_col_lower })
}

fn note_schema_mutation(database: &EngineDatabase, parsed: &[SqlStatement]) {
    let mut invalidate = false;
    for stmt in parsed {
        match stmt {
            SqlStatement::CreateTable(create) => {
                invalidate = true;
                if create.temporary {
                    database.has_temp_schema.set(true);
                }
            }
            SqlStatement::CreateIndex(_)
            | SqlStatement::CreateView(_)
            | SqlStatement::CreateVirtualTable(_)
            | SqlStatement::Drop(_)
            | SqlStatement::AlterTable(_)
            | SqlStatement::Rollback(_) => {
                invalidate = true;
            }
            _ => {}
        }
    }
    if invalidate {
        database.schema_version.set(None);
    }
}

/// `fsqlite-core 0.4.10` preserves prior row changes on a mid-statement
/// constraint halt only when the DML statement AST carries `OR FAIL`, rolling
/// back its internal statement savepoint when `ON CONFLICT FAIL` is declared
/// solely in the table schema. Promote `INSERT`/`UPDATE` without an explicit
/// `OR` or `UPSERT` clause to `OR FAIL` when every constraint on the target
/// table resolves conflicts via `FAIL`.
fn rewrite_schema_fail_dml<'a>(
    database: &EngineDatabase,
    stmt: Option<&SqlStatement>,
    sql: std::borrow::Cow<'a, str>,
) -> std::borrow::Cow<'a, str> {
    let fail_tables = database.schema_fail_tables.borrow();
    if fail_tables.is_empty() {
        return sql;
    }
    let is_insert = match stmt {
        Some(SqlStatement::Insert(ins)) if ins.or_conflict.is_none() && ins.upsert.is_empty() => {
            if ins.table.schema.as_deref().is_some_and(|schema| {
                !schema.eq_ignore_ascii_case("main") && !schema.eq_ignore_ascii_case("temp")
            }) {
                return sql;
            }
            let Some(profile) = fail_tables.get(&ins.table.name.to_ascii_lowercase()) else {
                return sql;
            };
            if let Some(ipk) = &profile.auto_ipk_col_lower
                && (ins.columns.is_empty()
                    || ins.columns.iter().any(|col| col.eq_ignore_ascii_case(ipk)))
            {
                return sql;
            }
            true
        }
        Some(SqlStatement::Update(upd)) if upd.or_conflict.is_none() => {
            if upd.table.name.schema.as_deref().is_some_and(|schema| {
                !schema.eq_ignore_ascii_case("main") && !schema.eq_ignore_ascii_case("temp")
            }) {
                return sql;
            }
            let Some(profile) = fail_tables.get(&upd.table.name.name.to_ascii_lowercase()) else {
                return sql;
            };
            if let Some(ipk) = &profile.auto_ipk_col_lower
                && upd.assignments.iter().any(|assign| match &assign.target {
                    fsqlite_ast::AssignmentTarget::Column(col) => col.eq_ignore_ascii_case(ipk),
                    fsqlite_ast::AssignmentTarget::ColumnList(cols) => {
                        cols.iter().any(|col| col.eq_ignore_ascii_case(ipk))
                    }
                })
            {
                return sql;
            }
            false
        }
        _ => return sql,
    };
    let mut paren_depth = 0_usize;
    for tok in Lexer::tokenize(sql.as_ref()) {
        match tok.kind {
            TokenKind::LeftParen => paren_depth += 1,
            TokenKind::RightParen => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::KwInsert | TokenKind::KwUpdate
                if paren_depth == 0
                    && matches!(
                        (&tok.kind, is_insert),
                        (TokenKind::KwInsert, true) | (TokenKind::KwUpdate, false)
                    ) =>
            {
                let end = usize::try_from(tok.span.end).unwrap_or(usize::MAX);
                if let Some((head, tail)) = sql.as_ref().split_at_checked(end) {
                    let mut out = String::with_capacity(sql.len() + 8);
                    out.push_str(head);
                    out.push_str(" OR FAIL");
                    out.push_str(tail);
                    return std::borrow::Cow::Owned(out);
                }
                break;
            }
            _ => {}
        }
    }
    sql
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PreExecWalAction {
    None,
    Release,
    Acquire,
}

#[derive(Clone, Copy)]
struct WalLeasePlan {
    had_writes: bool,
    pre_exec: PreExecWalAction,
    expected_writes_if_open: bool,
}

fn statement_writes(statement: &SqlStatement) -> bool {
    match statement {
        SqlStatement::Begin(begin) => matches!(
            begin.mode,
            Some(fsqlite_ast::TransactionMode::Immediate | fsqlite_ast::TransactionMode::Exclusive)
        ),
        SqlStatement::Insert(_)
        | SqlStatement::Update(_)
        | SqlStatement::Delete(_)
        | SqlStatement::CreateTable(_)
        | SqlStatement::CreateIndex(_)
        | SqlStatement::CreateView(_)
        | SqlStatement::CreateVirtualTable(_)
        | SqlStatement::Drop(_)
        | SqlStatement::AlterTable(_) => true,
        _ => false,
    }
}

fn plan_wal_lease(was_in_txn: bool, had_writes: bool, parsed: &[SqlStatement]) -> WalLeasePlan {
    let mut in_txn = was_in_txn;
    let mut writes_in_txn = was_in_txn && had_writes;
    let mut release_before_exec = false;
    for statement in parsed {
        match statement {
            SqlStatement::Begin(_) => {
                in_txn = true;
                if statement_writes(statement) {
                    writes_in_txn = true;
                }
            }
            SqlStatement::Commit => {
                release_before_exec = true;
                in_txn = false;
                writes_in_txn = false;
            }
            SqlStatement::Rollback(rollback) if rollback.to_savepoint.is_none() => {
                release_before_exec = true;
                in_txn = false;
                writes_in_txn = false;
            }
            SqlStatement::Pragma(_) => {
                release_before_exec = true;
            }
            other if in_txn && statement_writes(other) => {
                writes_in_txn = true;
            }
            _ => {}
        }
    }
    let pre_exec = if release_before_exec {
        PreExecWalAction::Release
    } else if in_txn && writes_in_txn {
        PreExecWalAction::Acquire
    } else {
        PreExecWalAction::None
    };
    WalLeasePlan {
        had_writes,
        pre_exec,
        expected_writes_if_open: writes_in_txn,
    }
}

async fn is_wal_journal_mode(conn: &Connection) -> NativeResult<bool> {
    let rows = conn
        .query("PRAGMA journal_mode")
        .await
        .map_err(SqliteFailure::from)?;
    Ok(matches!(
        rows.first().and_then(|row| row.get(0)),
        Some(SqliteValue::Text(mode)) if mode.as_ref().eq_ignore_ascii_case("wal")
    ))
}

async fn current_busy_timeout_ms(conn: &Connection) -> NativeResult<u64> {
    let rows = conn
        .query("PRAGMA busy_timeout")
        .await
        .map_err(SqliteFailure::from)?;
    let raw = rows
        .first()
        .and_then(|row| row.get(0))
        .and_then(|value| match value {
            SqliteValue::Integer(ms) => Some(u64::try_from(*ms).unwrap_or(0)),
            _ => None,
        })
        .unwrap_or(0);
    Ok(raw.min(MAX_BUSY_TIMEOUT_MS))
}

async fn ensure_wal_write_lease(
    database: &EngineDatabase,
    operation_cx: &fsqlite_types::cx::Cx,
    budget: &OperationBudget,
) -> NativeResult<()> {
    if database.wal_write_lease.borrow().is_some() {
        return Ok(());
    }
    let Some(path) = database.writable_db_path() else {
        return Ok(());
    };
    if !is_wal_journal_mode(&database.conn).await? {
        return Ok(());
    }
    let timeout_ms = current_busy_timeout_ms(&database.conn).await?;
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        match ExternalWalWriteLease::try_acquire(path, operation_cx) {
            Ok(lease) => {
                *database.wal_write_lease.borrow_mut() = Some(lease);
                return Ok(());
            }
            Err(FrankenError::Busy) => {
                let now = Instant::now();
                if now >= deadline {
                    return Err(SqliteFailure::from(FrankenError::Busy));
                }
                if operation_cx.is_cancel_requested() || budget.expire_at(now) {
                    return Err(SqliteFailure::from(FrankenError::Interrupt));
                }
                let sleep_for = deadline
                    .saturating_duration_since(now)
                    .min(Duration::from_millis(2));
                std::thread::sleep(sleep_for);
            }
            Err(err) => return Err(SqliteFailure::from(err)),
        }
    }
}

async fn prepare_wal_lease_before_exec(
    database: &EngineDatabase,
    plan: WalLeasePlan,
    operation_cx: &fsqlite_types::cx::Cx,
    budget: &OperationBudget,
) -> NativeResult<()> {
    match plan.pre_exec {
        PreExecWalAction::Release => database.release_wal_write_lease()?,
        PreExecWalAction::Acquire => ensure_wal_write_lease(database, operation_cx, budget).await?,
        PreExecWalAction::None => {}
    }
    Ok(())
}

async fn reconcile_wal_lease_after_exec<T>(
    database: &EngineDatabase,
    plan: WalLeasePlan,
    outcome: NativeResult<T>,
    operation_cx: &fsqlite_types::cx::Cx,
    budget: &OperationBudget,
) -> NativeResult<T> {
    if !database.conn.in_transaction() {
        database.txn_has_writes.set(false);
        database.release_wal_write_lease()?;
        return outcome;
    }
    match outcome {
        Ok(value) => {
            database.txn_has_writes.set(plan.expected_writes_if_open);
            if plan.expected_writes_if_open {
                ensure_wal_write_lease(database, operation_cx, budget).await?;
            } else {
                database.release_wal_write_lease()?;
            }
            Ok(value)
        }
        Err(err) => {
            database.txn_has_writes.set(plan.had_writes);
            if plan.had_writes {
                let _ = ensure_wal_write_lease(database, operation_cx, budget).await;
            } else {
                let _ = database.release_wal_write_lease();
            }
            Err(err)
        }
    }
}

#[allow(clippy::too_many_lines)]
async fn execute_request(
    database: &EngineDatabase,
    request: Request,
    budget: &OperationBudget,
    operation_cx: &fsqlite_types::cx::Cx,
) -> NativeResult<Json> {
    match request {
        Request::Prepare { sql, .. } => {
            let admitted = admit_sql(&sql, true)?;
            let exec_sql = admitted.normalized_sql(&sql);
            let columns = if admitted.uses_native_prepare() {
                budget.reserve_database(database, true)?;
                let (statement, _) = prepare_with_dqs(&database.conn, exec_sql.as_ref()).await?;
                let mut cols = statement.column_names();
                if cols.is_empty() && admitted.is_dml() {
                    cols =
                        dml_returning_column_names(&database.conn, admitted.parsed.first()).await?;
                }
                cols
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
            let (rewritten_fts_sql, hidden_col_count) = if matches!(mode, QueryMode::Run) {
                (None, 0)
            } else {
                rewrite_unprojected_fts5_order_by_select(admitted.parsed.first())
            };
            let exec_sql = match rewritten_fts_sql {
                Some(rewritten) => std::borrow::Cow::Owned(rewritten),
                None => admitted.normalized_sql(&sql),
            };
            let exec_sql = rewrite_schema_fail_dml(database, admitted.parsed.first(), exec_sql);
            budget.reserve_lock_wait(admitted.requested_busy_timeout())?;
            let params = bind_parameters(&admitted.parameters, bindings, database.strict)?;
            let wal_plan = plan_wal_lease(
                database.conn.in_transaction(),
                database.txn_has_writes.get(),
                &admitted.parsed,
            );
            prepare_wal_lease_before_exec(database, wal_plan, operation_cx, budget).await?;
            let query_outcome: NativeResult<Json> = async {
                if !admitted.uses_native_prepare() {
                    if params.is_empty() {
                        // execute() retains the original DDL source for default
                        // expressions and CHECK constraints. The parameterized
                        // dispatcher does not publish that source in 0.4.10.
                        database
                            .conn
                            .execute(exec_sql.as_ref())
                            .await
                            .map_err(SqliteFailure::from)?;
                    } else {
                        database
                            .conn
                            .execute_with_params(exec_sql.as_ref(), &params)
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
                let (statement, prepared_sql) =
                    prepare_with_dqs(&database.conn, exec_sql.as_ref()).await?;
                if matches!(mode, QueryMode::Run) {
                    match statement.execute_with_params(&params).await {
                        Ok(_) => {}
                        Err(
                            FrankenError::NotImplemented(message) | FrankenError::Internal(message),
                        ) if admitted.is_dml()
                            && !params.is_empty()
                            && (message.contains("Placeholder(")
                                || message.contains("canonicalized numbered placeholder")) =>
                        {
                            // fsqlite-core 0.4.10's PreparedDmlDispatch::Deferred skips
                            // placeholder canonicalization on live virtual tables (such
                            // as FTS5), and Connection::execute_with_params re-enters the
                            // same prepared fast lane for simple VALUES inserts. Rewrite
                            // parameter tokens to canonical ?N ordinals and re-prepare.
                            let canonical_sql =
                                canonicalize_sql_parameter_tokens(prepared_sql.as_ref())?;
                            database
                                .conn
                                .prepare(&canonical_sql)
                                .await
                                .map_err(SqliteFailure::from)?
                                .execute_with_params(&params)
                                .await
                                .map_err(SqliteFailure::from)?;
                        }
                        Err(error) => return Err(SqliteFailure::from(error)),
                    }
                    return change_result(database, budget).await;
                }
                let mut columns = statement.column_names();
                if columns.is_empty() && admitted.is_dml() {
                    columns =
                        dml_returning_column_names(&database.conn, admitted.parsed.first()).await?;
                }
                let visible_col_count = if hidden_col_count > 0 {
                    let count = columns.len().saturating_sub(hidden_col_count);
                    columns.truncate(count);
                    Some(count)
                } else {
                    None
                };
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
                        limit_error =
                            Some("SQLite result exceeds the row or time limit".to_string());
                        return Err(FrankenError::Abort);
                    }
                    if matches!(mode, QueryMode::Get) && !rows.is_empty() {
                        return Ok(());
                    }
                    bytes += 3; // enclosing row brackets and its separator
                    let values = row.values();
                    let visible_values = visible_col_count
                        .map_or(values, |count| &values[..values.len().min(count)]);
                    match visible_values
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
                    match database
                        .conn
                        .query_with_params(exec_sql.as_ref(), &params)
                        .await
                    {
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
            .await;
            note_schema_mutation(database, &admitted.parsed);
            reconcile_wal_lease_after_exec(database, wal_plan, query_outcome, operation_cx, budget)
                .await
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
            let wal_plan = plan_wal_lease(
                database.conn.in_transaction(),
                database.txn_has_writes.get(),
                &admitted.parsed,
            );
            prepare_wal_lease_before_exec(database, wal_plan, operation_cx, budget).await?;
            let exec_outcome: NativeResult<Json> = async {
                if !admitted.statements.is_empty() {
                    let exec_sql = admitted.normalized_sql(&sql);
                    database
                        .conn
                        .execute_batch(exec_sql.as_ref())
                        .await
                        .map_err(SqliteFailure::from)?;
                }
                change_result(database, budget).await
            }
            .await;
            note_schema_mutation(database, &admitted.parsed);
            reconcile_wal_lease_after_exec(database, wal_plan, exec_outcome, operation_cx, budget)
                .await
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

    #[test]
    fn canonicalize_sql_parameter_tokens_preserves_literals_and_numbers_slots() {
        let sql = "/* ? :skip */ INSERT INTO foo$bar(\"col$1\", [?raw], `tick?`) VALUES (?, :same, '? :lit', :same, ?4, ?) -- ?\n";
        let canonical = canonicalize_sql_parameter_tokens(sql).expect("canonicalize SQL");
        assert_eq!(
            canonical,
            "/* ? :skip */ INSERT INTO foo$bar(\"col$1\", [?raw], `tick?`) VALUES (?1, ?2, '? :lit', ?2, ?4, ?5) -- ?\n"
        );
    }

    #[test]
    fn fts5_parameterized_insert_and_bm25_query_use_canonical_placeholders() {
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
        exec(
            &mut worker,
            1,
            "CREATE VIRTUAL TABLE chunks USING fts5(title, content, tokenize='porter unicode61')",
        );
        for (title, content) in [
            (
                "SQLite",
                "Transactions keep persistent session records consistent",
            ),
            ("Unrelated", "A completely different document"),
        ] {
            let run_result = worker
                .call(Command::Request(Request::Query {
                    id: 1,
                    sql: "INSERT INTO chunks(title, content) VALUES (?, ?)".into(),
                    bindings: Bindings::Positional(vec![json!(title), json!(content)]),
                    mode: QueryMode::Run,
                }))
                .expect("FTS5 parameterized insert");
            assert_eq!(run_result["changes"], json!(1));
        }
        let search = worker
            .call(Command::Request(Request::Query {
                id: 1,
                sql: "SELECT title FROM chunks WHERE chunks MATCH ? ORDER BY bm25(chunks)".into(),
                bindings: Bindings::Positional(vec![json!("transaction")]),
                mode: QueryMode::All,
            }))
            .expect("FTS5 parameterized bm25 query");
        assert_eq!(
            search,
            json!({ "columns": ["title"], "rows": [["SQLite"]] })
        );
        worker
            .call(Command::Request(Request::Close { id: 1 }))
            .expect("close");
    }

    #[test]
    fn prepared_dml_and_select_preserve_dqs_literals_and_reject_missing_tables() {
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
            .expect("open memory database");
        exec(
            &mut worker,
            1,
            "CREATE TABLE conflicts(id INTEGER PRIMARY KEY, value TEXT)",
        );
        let dml_sql = "INSERT INTO conflicts VALUES (9, \"DQS literal\") RETURNING id, value";
        let prepared = worker
            .call(Command::Request(Request::Prepare {
                id: 1,
                sql: dml_sql.into(),
            }))
            .expect("prepare DQS DML with RETURNING");
        assert_eq!(prepared["columns"], json!(["id", "value"]));
        let got = worker
            .call(Command::Request(Request::Query {
                id: 1,
                sql: dml_sql.into(),
                bindings: Bindings::Positional(Vec::new()),
                mode: QueryMode::Get,
            }))
            .expect("execute DQS DML with RETURNING");
        assert_eq!(
            got,
            json!({ "columns": ["id", "value"], "rows": [[9, "DQS literal"]] })
        );
        let run_sql = "INSERT INTO conflicts VALUES (10, \"DQS run literal\")";
        worker
            .call(Command::Request(Request::Prepare {
                id: 1,
                sql: run_sql.into(),
            }))
            .expect("prepare DQS DML without RETURNING");
        worker
            .call(Command::Request(Request::Query {
                id: 1,
                sql: run_sql.into(),
                bindings: Bindings::Positional(Vec::new()),
                mode: QueryMode::Run,
            }))
            .expect("run DQS DML without RETURNING");
        let rows = worker
            .call(Command::Request(Request::Query {
                id: 1,
                sql: "SELECT id, value, \"DQS select\" AS tag FROM conflicts ORDER BY id".into(),
                bindings: Bindings::Positional(Vec::new()),
                mode: QueryMode::All,
            }))
            .expect("select DQS literal");
        assert_eq!(
            rows["rows"],
            json!([
                [9, "DQS literal", "DQS select"],
                [10, "DQS run literal", "DQS select"]
            ])
        );
        let missing_table = worker
            .call(Command::Request(Request::Prepare {
                id: 1,
                sql: "SELECT * FROM \"database is locked\"".into(),
            }))
            .expect_err("missing quoted table must still fail at prepare");
        assert!(
            missing_table.message.contains("no such table"),
            "unexpected prepare failure: {}",
            missing_table.message
        );
        worker
            .call(Command::Request(Request::Close { id: 1 }))
            .expect("close");
    }

    #[test]
    fn worker_preserves_schema_level_on_conflict_fail_with_returning_and_run() {
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
            .expect("open memory database");
        exec(
            &mut worker,
            1,
            "CREATE TABLE schema_fail(id INTEGER UNIQUE ON CONFLICT FAIL); \
             CREATE TABLE schema_abort(id INTEGER UNIQUE);",
        );
        worker
            .call(Command::Request(Request::Query {
                id: 1,
                sql: "INSERT INTO schema_fail VALUES (9)".into(),
                bindings: Bindings::Positional(Vec::new()),
                mode: QueryMode::Run,
            }))
            .expect("seed schema_fail");
        worker
            .call(Command::Request(Request::Query {
                id: 1,
                sql: "INSERT INTO schema_abort VALUES (9)".into(),
                bindings: Bindings::Positional(Vec::new()),
                mode: QueryMode::Run,
            }))
            .expect("seed schema_abort");
        worker
            .call(Command::Request(Request::Query {
                id: 1,
                sql: "INSERT INTO schema_fail VALUES (1), (9), (2) RETURNING id".into(),
                bindings: Bindings::Positional(Vec::new()),
                mode: QueryMode::All,
            }))
            .expect_err("duplicate row must still reject statement");
        worker
            .call(Command::Request(Request::Query {
                id: 1,
                sql: "INSERT INTO schema_abort VALUES (1), (9), (2) RETURNING id".into(),
                bindings: Bindings::Positional(Vec::new()),
                mode: QueryMode::All,
            }))
            .expect_err("duplicate row in abort table must reject statement");
        let fail_rows = worker
            .call(Command::Request(Request::Query {
                id: 1,
                sql: "SELECT id FROM schema_fail ORDER BY id".into(),
                bindings: Bindings::Positional(Vec::new()),
                mode: QueryMode::Values,
            }))
            .expect("select schema_fail");
        assert_eq!(fail_rows["rows"], json!([[1], [9]]));
        let abort_rows = worker
            .call(Command::Request(Request::Query {
                id: 1,
                sql: "SELECT id FROM schema_abort ORDER BY id".into(),
                bindings: Bindings::Positional(Vec::new()),
                mode: QueryMode::Values,
            }))
            .expect("select schema_abort");
        assert_eq!(abort_rows["rows"], json!([[9]]));
        worker
            .call(Command::Request(Request::Close { id: 1 }))
            .expect("close");
    }
}
