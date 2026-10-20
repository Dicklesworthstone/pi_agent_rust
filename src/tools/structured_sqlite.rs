//! Read SQLite data from an immutable, workspace-admitted database image.
//!
//! The query engine never opens the user's database or any of its sidecars.
//! A bounded snapshot includes committed WAL frames and is checked again
//! before dispatch to the supervised worker. This also keeps query results
//! independent of later writes and prevents a read from checkpointing a DB.

use std::fmt::Write as _;
use std::fs::File;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use fsqlite::{Connection, FrankenError, Row, SqliteValue};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::error::{Error, Result};

const MAGIC: &[u8] = b"SQLite format 3\0";
const MAX_SNAPSHOT_BYTES: usize = 100 * 1024 * 1024;
const MAX_QUERY_BYTES: usize = 32 * 1024;
const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
const MAX_CELL_BYTES: usize = 16 * 1024;
const MAX_NAME_BYTES: usize = 1024;
const MAX_SCHEMA_BYTES: usize = 64 * 1024;
const MAX_TABLES: usize = 500;
const MAX_COLUMNS: usize = 256;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct QueryRequest {
    pub source_path: String,
    table: Option<String>,
    key: Option<String>,
    predicate: Option<String>,
    order: Option<String>,
    query: Option<String>,
    row_offset: u64,
    row_limit: usize,
    paged: bool,
}

impl QueryRequest {
    pub(super) fn plain(source_path: String) -> Self {
        Self {
            source_path,
            table: None,
            key: None,
            predicate: None,
            order: None,
            query: None,
            row_offset: 0,
            row_limit: 20,
            paged: false,
        }
    }

    pub(super) fn has_selectors(&self) -> bool {
        self.table.is_some() || self.query.is_some() || self.paged
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct QueryOutcome {
    pub content: String,
    pub metadata: serde_json::Value,
}

#[derive(Serialize, Deserialize)]
struct WorkerRequest {
    request: QueryRequest,
    image: String,
    wal_frames: usize,
}

#[derive(Serialize, Deserialize)]
struct WorkerResponse {
    outcome: Option<QueryOutcome>,
    error: Option<String>,
}

fn invalid(message: impl std::fmt::Display) -> Error {
    Error::tool("read", format!("[SQLITE_READ] {message}"))
}

/// Split only after a recognized database suffix, preserving Windows drives
/// and directories whose names contain dots or colons.
pub(super) fn parse_path(path: &str) -> Result<Option<QueryRequest>> {
    let source_and_selector = path.split_once('?').map_or(path, |(source, _)| source);
    let source_end = source_and_selector.rmatch_indices('.').find_map(|(dot, _)| {
        let tail = &source_and_selector[dot + 1..];
        let extension = tail.split(':').next().unwrap_or(tail);
        ["sqlite", "sqlite3", "db", "db3"]
            .iter()
            .any(|wanted| extension.eq_ignore_ascii_case(wanted))
            .then_some(dot + 1 + extension.len())
    });
    let Some(source_end) = source_end else {
        return Ok(None);
    };
    let mut request = QueryRequest::plain(path[..source_end].to_string());
    let suffix = &source_and_selector[source_end..];
    if let Some(selector) = suffix.strip_prefix(':') {
        let (table, key) = selector
            .split_once(':')
            .map_or((selector, None), |(table, key)| (table, Some(key)));
        request.table = Some(decode_component(table)?);
        request.key = key.map(decode_component).transpose()?;
        if request.table.as_deref() == Some("") || request.key.as_deref() == Some("") {
            return Err(invalid("table names and row keys cannot be empty"));
        }
    }
    if let Some((_, query)) = path.split_once('?') {
        if query.len() > MAX_QUERY_BYTES {
            return Err(invalid("query parameters exceed 32 KiB"));
        }
        let mut seen = std::collections::HashSet::new();
        for field in query.split('&') {
            let (key, value) = field
                .split_once('=')
                .ok_or_else(|| invalid("query parameters must use name=value"))?;
            let key = if key == "query" { "q" } else { key };
            if !seen.insert(key) {
                return Err(invalid(format!("duplicate query parameter {key}")));
            }
            let value = decode_component(value)?;
            match key {
                "q" => request.query = Some(value),
                "where" => {
                    request.predicate = Some(value);
                    request.paged = true;
                }
                "order" => {
                    request.order = Some(value);
                    request.paged = true;
                }
                "limit" => {
                    request.row_limit = value
                        .parse::<usize>()
                        .ok()
                        .filter(|limit| (1..=500).contains(limit))
                        .ok_or_else(|| invalid("table limit must be between 1 and 500"))?;
                    request.paged = true;
                }
                "offset" => {
                    request.row_offset = value
                        .parse::<u64>()
                        .ok()
                        .filter(|offset| *offset <= i64::MAX as u64)
                        .ok_or_else(|| invalid("table offset must be a non-negative SQLite integer"))?;
                    request.paged = true;
                }
                _ => return Err(invalid(format!("unknown SQLite query parameter {key}"))),
            }
        }
    }
    if request.query.is_some() && (request.table.is_some() || request.paged) {
        return Err(invalid("q cannot be combined with table selectors or table pagination"));
    }
    if request.paged && (request.table.is_none() || request.key.is_some()) {
        return Err(invalid("where/order/limit/offset require a table without a row key"));
    }
    if let Some(query) = &request.query {
        validate_query(query)?;
    }
    if let Some(predicate) = &request.predicate {
        validate_predicate(predicate)?;
    }
    Ok(Some(request))
}

/// Percent decoding deliberately leaves '+' alone: it is SQL arithmetic.
fn decode_component(input: &str) -> Result<String> {
    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let high = (bytes[index + 1] as char).to_digit(16);
            let low = (bytes[index + 2] as char).to_digit(16);
            if let (Some(high), Some(low)) = (high, low) {
                output.push((high * 16 + low) as u8);
                index += 3;
                continue;
            }
        }
        output.push(bytes[index]);
        index += 1;
    }
    let output = String::from_utf8(output).map_err(|_| invalid("query must decode to UTF-8"))?;
    if output.contains('\0') {
        return Err(invalid("query parameters cannot contain NUL"));
    }
    Ok(output)
}

/// Tokenize the unquoted SQL surface. SQLite still parses the actual SQL;
/// this scanner prevents statement/parameter and structured-clause escapes.
fn sql_tokens(sql: &str) -> Result<Vec<String>> {
    if sql.is_empty() || sql.len() > MAX_QUERY_BYTES {
        return Err(invalid("SQL must contain 1 to 32768 bytes"));
    }
    let bytes = sql.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;
    let mut depth = 0_usize;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == 0 || (byte < 0x20 && !byte.is_ascii_whitespace()) {
            return Err(invalid("SQL contains a control character"));
        }
        if matches!(byte, b'\'' | b'"' | b'`' | b'[') {
            let close = if byte == b'[' { b']' } else { byte };
            index += 1;
            let mut terminated = false;
            while index < bytes.len() {
                if bytes[index] == close {
                    if close != b']' && bytes.get(index + 1) == Some(&close) {
                        index += 2;
                        continue;
                    }
                    index += 1;
                    terminated = true;
                    break;
                }
                index += 1;
            }
            if !terminated {
                return Err(invalid("unterminated SQL string or identifier"));
            }
            continue;
        }
        if matches!(byte, b';' | b'?' | b'$' | b'@' | b':')
            || (byte == b'-' && bytes.get(index + 1) == Some(&b'-'))
            || (byte == b'/' && bytes.get(index + 1) == Some(&b'*'))
        {
            return Err(invalid("SQL comments, statement separators, and unbound parameters are not supported"));
        }
        if byte == b'(' {
            depth += 1;
            if depth > 64 {
                return Err(invalid("SQL nesting exceeds 64 levels"));
            }
        } else if byte == b')' {
            depth = depth.checked_sub(1).ok_or_else(|| invalid("unbalanced SQL parentheses"))?;
        }
        if byte.is_ascii_alphabetic() || byte == b'_' {
            let start = index;
            index += 1;
            while index < bytes.len()
                && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_')
            {
                index += 1;
            }
            tokens.push(sql[start..index].to_ascii_uppercase());
        } else {
            index += 1;
        }
    }
    if depth != 0 {
        return Err(invalid("unbalanced SQL parentheses"));
    }
    Ok(tokens)
}

fn validate_query(sql: &str) -> Result<()> {
    let tokens = sql_tokens(sql)?;
    if !matches!(tokens.first().map(String::as_str), Some("SELECT" | "WITH" | "VALUES")) {
        return Err(invalid("q accepts one SELECT, WITH, or VALUES query"));
    }
    if tokens.iter().any(|token| {
        matches!(token.as_str(), "ATTACH" | "DETACH" | "PRAGMA" | "VACUUM" | "LOAD_EXTENSION" | "READFILE" | "WRITEFILE" | "FSDIR")
    }) {
        return Err(invalid("query attempts to access database configuration or external files"));
    }
    Ok(())
}

fn validate_predicate(sql: &str) -> Result<()> {
    let tokens = sql_tokens(sql)?;
    if tokens.iter().any(|token| {
        matches!(token.as_str(), "SELECT" | "WITH" | "VALUES" | "UNION" | "INTERSECT" | "EXCEPT" | "ORDER" | "LIMIT" | "OFFSET" | "GROUP" | "HAVING" | "WINDOW" | "RETURNING" | "ATTACH" | "DETACH" | "PRAGMA" | "LOAD_EXTENSION" | "READFILE" | "WRITEFILE" | "FSDIR")
    }) {
        return Err(invalid("where must be a row predicate; use q for a complete SELECT query"));
    }
    Ok(())
}

pub(super) fn is_sqlite(bytes: &[u8]) -> bool {
    bytes.starts_with(MAGIC)
}

struct SourceFile {
    path: PathBuf,
    file: File,
    bytes: Vec<u8>,
}

fn read_source(path: PathBuf, file: File, max_bytes: usize) -> Result<SourceFile> {
    let bytes = super::read_open_file_capped_at(&file, max_bytes as u64)
        .map_err(|error| invalid(format!("cannot capture bounded SQLite source: {error}")))?;
    Ok(SourceFile { path, file, bytes })
}

fn open_sidecar(path: &Path, suffix: &str, roots: &[PathBuf]) -> Result<Option<File>> {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    match super::open_regular_file_within_roots_with(Path::new(&name), roots, || {}) {
        Ok(file) => Ok(Some(file)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(invalid(format!("cannot admit SQLite {suffix} sidecar: {error}"))),
    }
}

fn ensure_same_source(source: &SourceFile, roots: &[PathBuf]) -> Result<()> {
    let current = super::open_regular_file_within_roots_with(&source.path, roots, || {})
        .map_err(|error| invalid(format!("SQLite source changed while being read: {error}")))?;
    let original_meta = source.file.metadata().map_err(|error| invalid(error))?;
    let current_meta = current.metadata().map_err(|error| invalid(error))?;
    if !super::opened_file_matches_metadata_snapshot(&original_meta, &current_meta) {
        return Err(invalid("SQLite source was replaced while being read; retry"));
    }
    let current_bytes = super::read_open_file_capped_at(&current, MAX_SNAPSHOT_BYTES as u64)
        .map_err(|error| invalid(error))?;
    if Sha256::digest(&current_bytes) != Sha256::digest(&source.bytes) {
        return Err(invalid("SQLite source changed while being read; retry after the writer settles"));
    }
    Ok(())
}

fn ensure_no_hot_journal(path: &Path, roots: &[PathBuf]) -> Result<()> {
    let Some(journal) = open_sidecar(path, "-journal", roots)? else {
        return Ok(());
    };
    // PERSIST mode keeps the allocation after commit but zeros its header.
    // A nonzero or partial header is ambiguous without recovery/locking;
    // refuse it instead of interpreting uncommitted main-file pages.
    let length = journal.metadata().map_err(invalid)?.len();
    if length == 0 {
        return Ok(());
    }
    let mut header = [0_u8; 8];
    let mut read = 0;
    while read < header.len() {
        let count = super::positioned_file_read(&journal, &mut header[read..], read as u64)
            .map_err(invalid)?;
        if count == 0 {
            break;
        }
        read += count;
    }
    if read != header.len() || header != [0; 8] {
        return Err(invalid("database has an active or unrecovered rollback journal; let its owner finish or recover it before reading"));
    }
    Ok(())
}

fn snapshot(file: File, path: &Path, roots: &[PathBuf]) -> Result<(Vec<u8>, usize)> {
    let main = read_source(path.to_path_buf(), file, MAX_SNAPSHOT_BYTES)?;
    if !is_sqlite(&main.bytes) {
        return Err(invalid("file does not contain a SQLite format 3 database"));
    }
    ensure_no_hot_journal(path, roots)?;
    let wal = if let Some(file) = open_sidecar(path, "-wal", roots)? {
        let mut wal_path = path.as_os_str().to_owned();
        wal_path.push("-wal");
        Some(read_source(
            PathBuf::from(wal_path),
            file,
            MAX_SNAPSHOT_BYTES.saturating_sub(main.bytes.len()),
        )?)
    } else {
        None
    };
    // A second content/identity pass admits only a stable pair. Reading only
    // the main file would silently omit recent commits from a live WAL DB.
    ensure_same_source(&main, roots)?;
    if let Some(wal) = &wal {
        ensure_same_source(wal, roots)?;
    } else if open_sidecar(path, "-wal", roots)?.is_some() {
        return Err(invalid("SQLite WAL appeared while being read; retry"));
    }
    ensure_no_hot_journal(path, roots)?;
    apply_wal(main.bytes, wal.as_ref().map_or(&[], |wal| wal.bytes.as_slice()))
}

fn be32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes([bytes[offset], bytes[offset + 1], bytes[offset + 2], bytes[offset + 3]])
}

fn checksum(bytes: &[u8], little_endian: bool, mut state: [u32; 2]) -> [u32; 2] {
    for pair in bytes.chunks_exact(8) {
        let word = |offset| {
            let value = [pair[offset], pair[offset + 1], pair[offset + 2], pair[offset + 3]];
            if little_endian { u32::from_le_bytes(value) } else { u32::from_be_bytes(value) }
        };
        state[0] = state[0].wrapping_add(word(0)).wrapping_add(state[1]);
        state[1] = state[1].wrapping_add(word(4)).wrapping_add(state[0]);
    }
    state
}

/// SQLite's documented WAL checksum/commit algorithm. Only frames through
/// the last complete, checksummed commit enter the private database image.
fn apply_wal(mut image: Vec<u8>, wal: &[u8]) -> Result<(Vec<u8>, usize)> {
    if image.len() < 100 || !is_sqlite(&image) {
        return Err(invalid("incomplete SQLite database header"));
    }
    let page_size = u16::from_be_bytes([image[16], image[17]]);
    let page_size = if page_size == 1 { 65536 } else { usize::from(page_size) };
    if !(512..=65536).contains(&page_size) || !page_size.is_power_of_two() || image.len() % page_size != 0 {
        return Err(invalid("invalid SQLite page size or partial database page"));
    }
    if wal.is_empty() {
        return Ok((image, 0));
    }
    if wal.len() < 32 || !matches!(be32(wal, 0), 0x377f_0682 | 0x377f_0683) {
        return Err(invalid("invalid WAL header"));
    }
    if image[18..20] != [2, 2] || be32(wal, 4) != 3_007_000 || be32(wal, 8) as usize != page_size {
        return Err(invalid("WAL does not match the database format"));
    }
    let little_endian = be32(wal, 0) == 0x377f_0682;
    let mut state = checksum(&wal[..24], little_endian, [0, 0]);
    if state != [be32(wal, 24), be32(wal, 28)] {
        return Err(invalid("WAL header checksum mismatch"));
    }
    let frame_size = page_size + 24;
    let mut committed_frames = 0;
    let mut committed_pages = 0;
    for (index, frame) in wal[32..].chunks_exact(frame_size).enumerate() {
        if frame[8..16] != wal[16..24] {
            break;
        }
        let next = checksum(&frame[..8], little_endian, state);
        let next = checksum(&frame[24..], little_endian, next);
        if next != [be32(frame, 16), be32(frame, 20)] {
            break;
        }
        let page = be32(frame, 0) as usize;
        let pages = be32(frame, 4) as usize;
        if page == 0 || page > MAX_SNAPSHOT_BYTES / page_size || pages > MAX_SNAPSHOT_BYTES / page_size {
            return Err(invalid("WAL page exceeds the 100 MiB snapshot limit"));
        }
        state = next;
        if pages != 0 {
            committed_frames = index + 1;
            committed_pages = pages;
        }
    }
    if committed_frames == 0 {
        return Ok((image, 0));
    }
    image.resize(committed_pages * page_size, 0);
    for frame in wal[32..].chunks_exact(frame_size).take(committed_frames) {
        let page = be32(frame, 0) as usize;
        if page <= committed_pages {
            image[(page - 1) * page_size..page * page_size].copy_from_slice(&frame[24..]);
        }
    }
    // Commit frames carry the authoritative size even when page 1 was not
    // rewritten. The imported image no longer has a WAL companion.
    image[28..32].copy_from_slice(&(committed_pages as u32).to_be_bytes());
    let counter = image[24..28].to_vec();
    image[92..96].copy_from_slice(&counter);
    Ok((image, committed_frames))
}

pub(super) async fn execute(
    request: QueryRequest,
    file: File,
    path: PathBuf,
    roots: Vec<PathBuf>,
) -> Result<QueryOutcome> {
    let (image, wal_frames) = asupersync::runtime::spawn_blocking_io(move || snapshot(file, &path, &roots)).await?;
    let wire = WorkerRequest {
        request,
        image: base64::engine::general_purpose::STANDARD.encode(image),
        wal_frames,
    };
    let bytes = super::structured_json::run_worker(
        super::structured_json::WorkerKind::Sqlite,
        serde_json::to_vec(&wire).map_err(|error| invalid(error))?,
    ).await?;
    let response: WorkerResponse = serde_json::from_slice(&bytes)
        .map_err(|error| invalid(format!("invalid SQLite worker response: {error}")))?;
    response.outcome.ok_or_else(|| invalid(response.error.unwrap_or_else(|| "query worker returned no result".to_string())))
}

pub(super) fn run_sqlite_worker() -> i32 {
    if let Err(error) = super::structured_json::set_worker_limits() {
        eprintln!("Cannot bound SQLite query worker: {error}");
        return 1;
    }
    let outcome = (|| -> Result<QueryOutcome> {
        let mut bytes = Vec::new();
        std::io::stdin().lock().take(150 * 1024 * 1024 + 1).read_to_end(&mut bytes)
            .map_err(|error| invalid(error))?;
        if bytes.len() > 150 * 1024 * 1024 {
            return Err(invalid("SQLite worker input exceeds 150 MiB"));
        }
        let wire: WorkerRequest = serde_json::from_slice(&bytes).map_err(|error| invalid(error))?;
        drop(bytes);
        let image = base64::engine::general_purpose::STANDARD.decode(&wire.image)
            .map_err(|error| invalid(error))?;
        let WorkerRequest { request, image: encoded_image, wal_frames } = wire;
        drop(encoded_image);
        if image.len() > MAX_SNAPSHOT_BYTES {
            return Err(invalid("SQLite image exceeds 100 MiB"));
        }
        // The engine's nested futures need more stack than the default
        // spawned-thread stack. Keep this independent of sqlite-sessions:
        // native database reading is available in minimal builds too.
        let worker = std::thread::Builder::new()
            .name("pi-sqlite-read".to_string())
            .stack_size(16 * 1024 * 1024)
            .spawn(move || futures::executor::block_on(query_image(&image, &request, wal_frames)))
            .map_err(invalid)?;
        worker.join().map_err(|_| invalid("SQLite query worker panicked"))?
    })();
    let response = match outcome {
        Ok(outcome) => WorkerResponse { outcome: Some(outcome), error: None },
        Err(error) => WorkerResponse { outcome: None, error: Some(error.to_string()) },
    };
    match serde_json::to_writer(std::io::stdout().lock(), &response) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("Cannot write SQLite query result: {error}");
            1
        }
    }
}

fn quote_identifier(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn text_cell(row: &Row, index: usize) -> Result<String> {
    match row.get(index) {
        Some(SqliteValue::Text(value)) => Ok(value.to_string()),
        Some(SqliteValue::Null) => Ok(String::new()),
        _ => Err(invalid("database returned invalid schema metadata")),
    }
}

fn integer_cell(row: &Row, index: usize) -> Result<i64> {
    match row.get(index) {
        Some(SqliteValue::Integer(value)) => Ok(*value),
        _ => Err(invalid("database returned invalid integer metadata")),
    }
}

fn sql_error(error: FrankenError) -> Error {
    invalid(format!("query failed: {error}"))
}

struct QueryRows {
    columns: Vec<String>,
    rows: Vec<Vec<String>>,
    has_more: bool,
    cells_truncated: bool,
    names_truncated: bool,
}

fn bounded_metadata(text: &str, limit: usize, truncated: &mut bool) -> String {
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    if end == text.len() {
        text.to_owned()
    } else {
        *truncated = true;
        format!("{}… [metadata truncated]", &text[..end])
    }
}

fn query_output(content: String, metadata: serde_json::Value) -> Result<QueryOutcome> {
    if content.len() > MAX_OUTPUT_BYTES {
        return Err(invalid("rendered result exceeds 8 MiB; select fewer or smaller columns"));
    }
    Ok(QueryOutcome { content, metadata })
}

fn render_cell(value: &SqliteValue, truncated: &mut bool) -> String {
    let text = match value {
        SqliteValue::Null => "NULL".to_string(),
        SqliteValue::Integer(value) => value.to_string(),
        SqliteValue::Float(value) => value.to_string(),
        SqliteValue::Text(value) => value.to_string(),
        SqliteValue::Blob(value) => return format!("<BLOB {} bytes>", value.len()),
    };
    let mut end = text.len().min(MAX_CELL_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    if end != text.len() {
        *truncated = true;
        format!("{}… [cell truncated]", &text[..end])
    } else {
        text
    }
}

async fn bounded_rows(conn: &Connection, sql: &str, params: &[SqliteValue], limit: usize) -> Result<QueryRows> {
    let statement = conn.prepare(sql).await.map_err(sql_error)?;
    let columns = statement.column_names();
    if columns.len() > MAX_COLUMNS {
        return Err(invalid("result has more than 256 columns; select fewer columns"));
    }
    let mut names_truncated = false;
    let columns = columns.iter()
        .map(|name| bounded_metadata(name, MAX_NAME_BYTES, &mut names_truncated))
        .collect();
    let mut result = QueryRows { columns, rows: Vec::new(), has_more: false, cells_truncated: false, names_truncated };
    let mut output_bytes = 0_usize;
    let mut stopped = false;
    let execution = statement.query_with_params_for_each(params, |row| {
        if result.rows.len() >= limit {
            result.has_more = true;
            stopped = true;
            return Err(FrankenError::Abort);
        }
        let cells: Vec<String> = row.values().iter()
            .map(|cell| render_cell(cell, &mut result.cells_truncated))
            .collect();
        let row_bytes = cells.iter().map(String::len).sum::<usize>();
        if row_bytes > MAX_OUTPUT_BYTES.saturating_sub(output_bytes) {
            result.has_more = true;
            stopped = true;
            return Err(FrankenError::Abort);
        }
        output_bytes += row_bytes;
        result.rows.push(cells);
        Ok(())
    }).await;
    if let Err(error) = execution {
        if !stopped || !matches!(error, FrankenError::Abort) {
            return Err(sql_error(error));
        }
    }
    Ok(result)
}

fn markdown_cell(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            '|' => escaped.push_str("\\|"),
            '\\' => escaped.push_str("\\\\"),
            character if character.is_control() => escaped.push('�'),
            character => escaped.push(character),
        }
    }
    escaped
}

fn render_rows(result: &QueryRows) -> Result<String> {
    let mut text = String::new();
    if result.rows.is_empty() {
        return Ok("(no rows)\n".to_string());
    }
    if result.columns.len() <= 8 {
        writeln!(text, "| {} |", result.columns.iter().map(|column| markdown_cell(column)).collect::<Vec<_>>().join(" | ")).ok();
        writeln!(text, "| {} |", vec!["---"; result.columns.len()].join(" | ")).ok();
        for row in &result.rows {
            writeln!(text, "| {} |", row.iter().map(|cell| markdown_cell(cell)).collect::<Vec<_>>().join(" | ")).ok();
            if text.len() > MAX_OUTPUT_BYTES {
                return Err(invalid("rendered rows exceed 8 MiB; select fewer or smaller columns"));
            }
        }
    } else {
        for (index, row) in result.rows.iter().enumerate() {
            writeln!(text, "### Row {}", index + 1).ok();
            for (column, cell) in result.columns.iter().zip(row) {
                writeln!(text, "{}: {}", markdown_cell(column), markdown_cell(cell)).ok();
            }
            text.push('\n');
            if text.len() > MAX_OUTPUT_BYTES {
                return Err(invalid("rendered rows exceed 8 MiB; select fewer or smaller columns"));
            }
        }
    }
    if result.cells_truncated {
        text.push_str("\n[Long cells were truncated at 16 KiB. Select substr(column, start, length) to inspect them.]\n");
    }
    if result.names_truncated {
        text.push_str("\n[Column labels were truncated at 1 KiB. Use short AS aliases to identify columns.]\n");
    }
    Ok(text)
}

async fn query_image(image: &[u8], request: &QueryRequest, wal_frames: usize) -> Result<QueryOutcome> {
    let conn = Connection::import_bytes(image).await.map_err(sql_error)?;
    conn.execute("PRAGMA query_only = ON").await.map_err(sql_error)?;
    let mut metadata = serde_json::json!({
        "contentType": "sqlite",
        "extractor": "sqlite",
        "rendered": true,
        "readOnly": true,
        "sourceBytes": image.len(),
        "snapshot": true,
        "walFrames": wal_frames,
    });
    if let Some(query) = &request.query {
        validate_query(query)?;
        let result = bounded_rows(&conn, &format!("SELECT * FROM ({query}) AS pi_read_query LIMIT 1001"), &[], 1000).await?;
        let mut content = render_rows(&result)?;
        if result.has_more {
            content.push_str("\n[Result limited to 1000 rows or 8 MiB. Add LIMIT/OFFSET to q to continue.]\n");
        }
        metadata["rowCount"] = serde_json::json!(result.rows.len());
        metadata["hasMoreRows"] = serde_json::json!(result.has_more);
        metadata["cellsTruncated"] = serde_json::json!(result.cells_truncated);
        metadata["namesTruncated"] = serde_json::json!(result.names_truncated);
        return query_output(content, metadata);
    }
    let Some(table) = &request.table else {
        let schema = conn.query("SELECT name, type FROM sqlite_master WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite_%' ORDER BY name LIMIT 501").await.map_err(sql_error)?;
        let mut names_truncated = false;
        let mut content = String::from("# SQLite tables and views\n\n| Name | Type |\n| --- | --- |\n");
        for row in schema.iter().take(MAX_TABLES) {
            let name = bounded_metadata(&text_cell(row, 0)?, MAX_NAME_BYTES, &mut names_truncated);
            writeln!(content, "| {} | {} |", markdown_cell(&name), markdown_cell(&text_cell(row, 1)?)).ok();
        }
        if schema.is_empty() {
            content.push_str("\n(no user tables or views)\n");
        }
        if schema.len() > MAX_TABLES {
            content.push_str("\n[Showing the first 500 tables/views; use q to query sqlite_master for more.]\n");
        }
        content.push_str("\nUse database.sqlite:table for schema and sample rows, :table:key for a row, or ?q=SELECT... to query.\n");
        metadata["tableCount"] = serde_json::json!(schema.len().min(MAX_TABLES));
        metadata["hasMoreTables"] = serde_json::json!(schema.len() > MAX_TABLES);
        metadata["namesTruncated"] = serde_json::json!(names_truncated);
        return query_output(content, metadata);
    };
    let table_schema = conn.query_with_params("SELECT name, type, sql FROM sqlite_master WHERE type IN ('table', 'view') AND name = ?1 COLLATE NOCASE LIMIT 2", &[SqliteValue::Text(table.clone().into())]).await.map_err(sql_error)?;
    let row = table_schema.first().ok_or_else(|| invalid(format!("table or view {table:?} does not exist")))?;
    let table = text_cell(row, 0)?;
    let object_type = text_cell(row, 1)?;
    let mut schema_truncated = false;
    let create_sql = bounded_metadata(&text_cell(row, 2)?, MAX_SCHEMA_BYTES, &mut schema_truncated);
    let quoted = quote_identifier(&table);
    let columns = conn.query(&format!("PRAGMA table_info({quoted})")).await.map_err(sql_error)?;
    if columns.len() > MAX_COLUMNS {
        return Err(invalid("table has more than 256 columns; use q to select fewer columns"));
    }
    let names: Vec<String> = columns.iter().map(|row| text_cell(row, 1)).collect::<Result<_>>()?;
    let mut primary = columns.iter().filter_map(|row| {
        match integer_cell(row, 5) {
            Ok(position) if position > 0 => Some(Ok((position, row))),
            Ok(_) => None,
            Err(error) => Some(Err(error)),
        }
    }).collect::<Result<Vec<_>>>()?;
    primary.sort_by_key(|(position, _)| *position);
    let (sql, params, limit) = if let Some(key) = &request.key {
        if primary.len() > 1 {
            return Err(invalid("table has a composite primary key; use where or q to specify all key columns"));
        }
        let (column, value) = if let Some((_, row)) = primary.first() {
            let name = text_cell(row, 1)?;
            let kind = text_cell(row, 2)?;
            let value = if kind.to_ascii_uppercase().contains("INT") {
                SqliteValue::Integer(key.parse::<i64>().map_err(|_| invalid("row key must be a signed 64-bit integer"))?)
            } else {
                SqliteValue::Text(key.clone().into())
            };
            (quote_identifier(&name), value)
        } else {
            if object_type == "view" {
                return Err(invalid("views have no rowid; use where or q"));
            }
            let alias = ["rowid", "_rowid_", "oid"].into_iter()
                .find(|alias| !names.iter().any(|name| name.eq_ignore_ascii_case(alias)))
                .ok_or_else(|| invalid("all rowid aliases are shadowed; use where or q"))?;
            (quote_identifier(alias), SqliteValue::Integer(key.parse::<i64>().map_err(|_| invalid("rowid must be a signed 64-bit integer"))?))
        };
        (format!("SELECT * FROM {quoted} WHERE {column} = ?1 LIMIT 2"), vec![value], 1)
    } else {
        let mut sql = format!("SELECT * FROM {quoted}");
        if let Some(predicate) = &request.predicate {
            validate_predicate(predicate)?;
            write!(sql, " WHERE ({predicate})").ok();
        }
        if let Some(order) = &request.order {
            let mut parts = Vec::new();
            for part in order.split(',') {
                let (column, direction) = part.rsplit_once(':').unwrap_or((part, "asc"));
                let actual = names.iter().find(|name| name.eq_ignore_ascii_case(column.trim()))
                    .ok_or_else(|| invalid(format!("unknown order column {column:?}")))?;
                let direction = if direction.eq_ignore_ascii_case("asc") { "ASC" }
                    else if direction.eq_ignore_ascii_case("desc") { "DESC" }
                    else { return Err(invalid("order direction must be asc or desc")); };
                parts.push(format!("{} {direction}", quote_identifier(actual)));
            }
            write!(sql, " ORDER BY {}", parts.join(", ")).ok();
        } else if !primary.is_empty() {
            let keys = primary.iter().map(|(_, row)| text_cell(row, 1).map(|name| quote_identifier(&name))).collect::<Result<Vec<_>>>()?;
            write!(sql, " ORDER BY {}", keys.join(", ")).ok();
        }
        let limit = if request.paged { request.row_limit } else { 5 };
        sql.push_str(" LIMIT ?1 OFFSET ?2");
        (sql, vec![SqliteValue::Integer((limit + 1) as i64), SqliteValue::Integer(request.row_offset as i64)], limit)
    };
    let result = bounded_rows(&conn, &sql, &params, limit).await?;
    let mut names_truncated = result.names_truncated;
    let display_table = bounded_metadata(&table, MAX_NAME_BYTES, &mut names_truncated);
    let mut content = format!("# {}\n\n```sql\n{}\n```\n\n", markdown_cell(&display_table), create_sql);
    content.push_str(&render_rows(&result)?);
    if result.has_more && request.key.is_none() {
        let next = request.row_offset.saturating_add(result.rows.len() as u64);
        writeln!(content, "\n[More rows available. Continue with :table?limit={limit}&offset={next}, preserving any where/order parameters.]").ok();
        metadata["nextRowOffset"] = serde_json::json!(next);
    }
    metadata["table"] = serde_json::json!(display_table);
    metadata["rowCount"] = serde_json::json!(result.rows.len());
    metadata["rowOffset"] = serde_json::json!(request.row_offset);
    metadata["rowLimit"] = serde_json::json!(limit);
    metadata["hasMoreRows"] = serde_json::json!(result.has_more);
    metadata["cellsTruncated"] = serde_json::json!(result.cells_truncated);
    metadata["namesTruncated"] = serde_json::json!(names_truncated);
    metadata["schemaTruncated"] = serde_json::json!(schema_truncated);
    query_output(content, metadata)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn on_engine_thread<T: Send + 'static>(run: impl FnOnce() -> T + Send + 'static) -> T {
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(run)
            .unwrap()
            .join()
            .unwrap()
    }

    async fn image_fixture() -> Vec<u8> {
        let conn = Connection::open(":memory:").await.unwrap();
        conn.execute_batch(
            "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT, active INTEGER, payload BLOB);\n\
             INSERT INTO users VALUES(1, 'Ada', 1, X'000102');\n\
             INSERT INTO users VALUES(2, 'Grace', 0, NULL);\n\
             INSERT INTO users VALUES(9223372036854775807, 'Max', 1, X'FF');\n\
             CREATE TABLE events(message TEXT);\n\
             INSERT INTO events VALUES('first'), ('second');\n\
             CREATE TABLE pairs(a TEXT, b TEXT, PRIMARY KEY(a,b));\n\
             INSERT INTO pairs VALUES('one','two');\n\
             CREATE VIEW names AS SELECT name FROM users;",
        ).await.unwrap();
        conn.export_bytes().await.unwrap()
    }

    #[test]
    fn selectors_preserve_drives_identifiers_and_arithmetic() {
        let request = parse_path("C:\\work.db\\data.SQLITE:odd%3Atable:key%3Avalue").unwrap().unwrap();
        assert_eq!(request.source_path, "C:\\work.db\\data.SQLITE");
        assert_eq!(request.table.as_deref(), Some("odd:table"));
        assert_eq!(request.key.as_deref(), Some("key:value"));
        let request = parse_path("data.sqlite?q=SELECT+1%2B2").unwrap().unwrap();
        assert_eq!(request.query.as_deref(), Some("SELECT+1+2"));
        assert!(parse_path("data.sqlite3?query=SELECT 1&limit=2").is_err());
        assert!(parse_path("data.db:users:1?where=active=1").is_err());
        assert!(parse_path("data.db:users?limit=501").is_err());
        assert!(parse_path("data.db:users?offset=-1").is_err());
        assert!(parse_path("data.db?q=SELECT 1&query=SELECT 2").is_err());
        assert!(parse_path("plain.json?q=.").unwrap().is_none());
    }

    #[test]
    fn sql_boundaries_refuse_mutations_and_clause_escapes() {
        for query in [
            "DELETE FROM users", "PRAGMA query_only=0", "ATTACH 'secret.db' AS secret",
            "SELECT 1; DELETE FROM users", "SELECT ?1", "SELECT readfile('/etc/passwd')",
            "SELECT 1 -- trailing comment", "SELECT 1 /* comment */", "SELECT (1",
        ] {
            assert!(validate_query(query).is_err(), "{query}");
        }
        assert!(validate_query("WITH chosen AS (SELECT 1 AS n) SELECT n+1 FROM chosen").is_ok());
        assert!(validate_query("SELECT 'semi; colon, ? and -- inside a value'").is_ok());
        for predicate in ["1) OR 1=1", "1 UNION SELECT 1", "active=1 LIMIT 1000000", "1 --", "id IN (SELECT id FROM users)"] {
            assert!(validate_predicate(predicate).is_err(), "{predicate}");
        }
        assert!(validate_predicate("name='ORDER' AND (active=1 OR id>5)").is_ok());
    }

    #[test]
    fn native_database_browsing_queries_and_full_width_integer_keys() {
        on_engine_thread(|| futures::executor::block_on(async {
            let image = image_fixture().await;
            let listing = query_image(&image, &QueryRequest::plain("data.sqlite".into()), 0).await.unwrap();
            assert!(listing.content.contains("users"));
            assert!(listing.content.contains("names"));
            assert!(listing.content.contains("view"));
            let request = parse_path("data.sqlite:users:9223372036854775807").unwrap().unwrap();
            let row = query_image(&image, &request, 0).await.unwrap();
            assert!(row.content.contains("9223372036854775807"));
            assert!(row.content.contains("Max"));
            assert!(row.content.contains("<BLOB 1 bytes>"));
            assert!(!row.content.contains("Ada"));
            let request = parse_path("data.sqlite:events:2").unwrap().unwrap();
            let row = query_image(&image, &request, 0).await.unwrap();
            assert!(row.content.contains("second"));
            assert!(!row.content.contains("first"));
            let request = parse_path("data.sqlite:users?where=active=1&order=id:desc&limit=1").unwrap().unwrap();
            let page = query_image(&image, &request, 0).await.unwrap();
            assert!(page.content.contains("Max"));
            assert!(!page.content.contains("Ada"));
            assert_eq!(page.metadata["nextRowOffset"], 1);
            assert_eq!(page.metadata["hasMoreRows"], true);
            let request = parse_path("data.sqlite:users?where=active=1&order=id:desc&limit=1&offset=1").unwrap().unwrap();
            let page = query_image(&image, &request, 0).await.unwrap();
            assert!(page.content.contains("Ada"));
            assert!(!page.content.contains("Max"));
            assert_eq!(page.metadata["hasMoreRows"], false);
            let request = parse_path("data.sqlite?q=SELECT name, id+1 AS next_id FROM users WHERE id=1").unwrap().unwrap();
            let projection = query_image(&image, &request, 0).await.unwrap();
            assert!(projection.content.contains("next_id"));
            assert!(projection.content.contains("Ada"));
            let request = parse_path("data.sqlite:users:2").unwrap().unwrap();
            assert!(query_image(&image, &request, 0).await.unwrap().content.contains("NULL"));
            let request = parse_path("data.sqlite:pairs:one").unwrap().unwrap();
            assert!(query_image(&image, &request, 0).await.unwrap_err().to_string().contains("composite"));
            let request = parse_path("data.sqlite:names:1").unwrap().unwrap();
            assert!(query_image(&image, &request, 0).await.unwrap_err().to_string().contains("views have no rowid"));
            // Queries operate on copies: another import still sees every row.
            let conn = Connection::import_bytes(&image).await.unwrap();
            let rows = conn.query("SELECT count(*) FROM users").await.unwrap();
            assert_eq!(integer_cell(&rows[0], 0).unwrap(), 3);
        }));
    }

    #[test]
    fn native_result_capture_limits_rows_and_marks_oversized_cells() {
        on_engine_thread(|| futures::executor::block_on(async {
            let image = image_fixture().await;
            let request = parse_path("data.sqlite?q=WITH RECURSIVE seq(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM seq WHERE n<1005) SELECT n FROM seq").unwrap().unwrap();
            let output = query_image(&image, &request, 0).await.unwrap();
            assert_eq!(output.metadata["rowCount"], 1000);
            assert_eq!(output.metadata["hasMoreRows"], true);
            let conn = Connection::import_bytes(&image).await.unwrap();
            let result = bounded_rows(&conn, "SELECT ?1 AS value", &[SqliteValue::Text("界".repeat(MAX_CELL_BYTES).into())], 1).await.unwrap();
            assert!(result.cells_truncated);
            assert!(render_rows(&result).unwrap().contains("cell truncated"));
        }));
    }

    #[test]
    fn database_metadata_cannot_expand_past_display_bounds() {
        on_engine_thread(|| futures::executor::block_on(async {
            let conn = Connection::open(":memory:").await.unwrap();
            let column = "界".repeat(MAX_NAME_BYTES);
            conn.execute(&format!(
                "CREATE TABLE large_schema({} TEXT DEFAULT '{}')",
                quote_identifier(&column),
                "x".repeat(MAX_SCHEMA_BYTES + 1),
            )).await.unwrap();
            conn.execute("INSERT INTO large_schema DEFAULT VALUES").await.unwrap();
            let image = conn.export_bytes().await.unwrap();
            let request = parse_path("data.sqlite:large_schema").unwrap().unwrap();
            let output = query_image(&image, &request, 0).await.unwrap();
            assert_eq!(output.metadata["schemaTruncated"], true);
            assert_eq!(output.metadata["namesTruncated"], true);
            assert_eq!(output.metadata["cellsTruncated"], true);
            assert!(output.content.contains("metadata truncated"));
            assert!(output.content.len() < 100 * 1024);
        }));
    }

    fn wal_fixture(page_size: usize, little_endian: bool, frames: &[(u32, u32, Vec<u8>)]) -> Vec<u8> {
        let mut wal = vec![0_u8; 32];
        let magic: u32 = if little_endian { 0x377f_0682 } else { 0x377f_0683 };
        wal[..4].copy_from_slice(&magic.to_be_bytes());
        wal[4..8].copy_from_slice(&3_007_000_u32.to_be_bytes());
        wal[8..12].copy_from_slice(&(page_size as u32).to_be_bytes());
        wal[16..24].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        let mut state = checksum(&wal[..24], little_endian, [0, 0]);
        wal[24..28].copy_from_slice(&state[0].to_be_bytes());
        wal[28..32].copy_from_slice(&state[1].to_be_bytes());
        for (page, commit_pages, data) in frames {
            assert_eq!(data.len(), page_size);
            let mut frame = vec![0_u8; 24];
            frame[..4].copy_from_slice(&page.to_be_bytes());
            frame[4..8].copy_from_slice(&commit_pages.to_be_bytes());
            frame[8..16].copy_from_slice(&wal[16..24]);
            state = checksum(&frame[..8], little_endian, state);
            state = checksum(data, little_endian, state);
            frame[16..20].copy_from_slice(&state[0].to_be_bytes());
            frame[20..24].copy_from_slice(&state[1].to_be_bytes());
            wal.extend_from_slice(&frame);
            wal.extend_from_slice(data);
        }
        wal
    }

    #[test]
    fn wal_applies_only_last_committed_prefix_and_checks_both_endian_formats() {
        let mut image = vec![0_u8; 1024];
        image[..16].copy_from_slice(MAGIC);
        image[16..18].copy_from_slice(&512_u16.to_be_bytes());
        image[18..20].copy_from_slice(&[2, 2]);
        image[512..].fill(1);
        for little_endian in [true, false] {
            let wal = wal_fixture(512, little_endian, &[
                (2, 2, vec![2; 512]),
                (2, 0, vec![3; 512]),
            ]);
            let (snapshot, frames) = apply_wal(image.clone(), &wal).unwrap();
            assert_eq!(frames, 1);
            assert_eq!(&snapshot[512..], &[2; 512]);
            let mut corrupt = wal.clone();
            corrupt[32 + 24] ^= 1;
            let (snapshot, frames) = apply_wal(image.clone(), &corrupt).unwrap();
            assert_eq!(frames, 0);
            assert_eq!(&snapshot[512..], &[1; 512]);
            let mut bad_header = wal.clone();
            bad_header[24] ^= 1;
            assert!(apply_wal(image.clone(), &bad_header).is_err());
            let mut reset_tail = wal.clone();
            reset_tail[32 + 536 + 8] ^= 1;
            assert_eq!(apply_wal(image.clone(), &reset_tail).unwrap().1, 1);
        }
    }

    #[test]
    fn snapshot_preserves_source_accepts_cold_journals_and_refuses_unrecovered_ones() {
        let image = on_engine_thread(|| futures::executor::block_on(image_fixture()));
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("data.sqlite");
        std::fs::write(&path, &image).unwrap();
        let journal = directory.path().join("data.sqlite-journal");
        std::fs::write(&journal, vec![0_u8; 512]).unwrap();
        let roots = vec![directory.path().to_path_buf()];
        let (captured, frames) = snapshot(File::open(&path).unwrap(), &path, &roots).unwrap();
        assert_eq!(captured, image);
        assert_eq!(frames, 0);
        assert_eq!(std::fs::read(&path).unwrap(), image);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 2);
        std::fs::write(&journal, [0xd9, 0xd5, 0x05, 0xf9, 0x20, 0xa1, 0x63, 0xd7]).unwrap();
        assert!(snapshot(File::open(&path).unwrap(), &path, &roots).unwrap_err().to_string().contains("rollback journal"));
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_refuses_wal_sidecars_outside_the_workspace() {
        use std::os::unix::fs::symlink;
        let image = on_engine_thread(|| futures::executor::block_on(image_fixture()));
        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let path = directory.path().join("data.sqlite");
        std::fs::write(&path, image).unwrap();
        let outside_wal = outside.path().join("other-wal");
        std::fs::write(&outside_wal, b"not admitted").unwrap();
        symlink(&outside_wal, directory.path().join("data.sqlite-wal")).unwrap();
        assert!(snapshot(File::open(&path).unwrap(), &path, &[directory.path().to_path_buf()]).is_err());
    }
}
