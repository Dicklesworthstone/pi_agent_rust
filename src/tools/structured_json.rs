//! Native jq-compatible Read queries and their resource-bounded worker owner.
//!
//! jaq deliberately has no interpreter fuel or interrupt hook. A filter can
//! recurse or allocate indefinitely inside one iterator step, so checking a
//! deadline between results is insufficient. Only the private Pi subprocess
//! evaluates filters; the parent retains process, pipe, and cancellation
//! ownership. Workers receive already admitted bytes and have no file loader.

use crate::agent_cx::{AgentChild, AgentCx};
use crate::error::{Error, Result};
use base64::Engine as _;
use jaq_core::data::{DataT, HasLut};
use jaq_core::load::{Arena, File, Loader};
use jaq_core::native::{self, bome, v};
use jaq_core::{Ctx, Lut, RunPtr, Vars};
use jaq_json::Val;
use jaq_std::ValT as _;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub(super) const MAX_INPUT_BYTES: usize = 10 * 1024 * 1024;
const MAX_QUERY_BYTES: usize = 16 * 1024;
const MAX_QUERY_PARAMETERS: usize = 64 * 1024;
const MAX_JSON_REQUEST: usize = 16 * 1024 * 1024;
const MAX_SQLITE_REQUEST: usize = 150 * 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 5 * 1024 * 1024;
const MAX_DIAGNOSTIC_BYTES: usize = 8 * 1024;
const MAX_JSON_DEPTH: usize = 256;
#[cfg(all(unix, not(target_os = "openbsd")))]
const MAX_WORKER_MEMORY: u64 = 1024 * 1024 * 1024;
const WORKER_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_ACTIVE_WORKERS: usize = 4;
static ACTIVE_WORKERS: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct QueryRequest {
    pub source_path: String,
    pub query: String,
    pub raw: bool,
    pub compact: bool,
    pub offset: Option<usize>,
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(super) struct QueryOutcome {
    pub content: String,
    pub metadata: serde_json::Value,
    pub is_error: bool,
}

#[derive(Deserialize, Serialize)]
struct WorkerRequest {
    selector: QueryRequest,
    source_base64: String,
}

fn invalid(message: impl Into<String>) -> Error {
    Error::tool("read", message)
}

/// Split at the first `?`, because jq's optional operator may occur later.
/// `+` is an arithmetic operator, never form-encoded whitespace in a filter.
pub(super) fn parse_path(path: &str) -> Result<Option<QueryRequest>> {
    let Some((source_path, parameters)) = path.split_once('?') else {
        return Ok(None);
    };
    let extension = source_path.rsplit('.').next().unwrap_or_default();
    if !["json", "jsonl", "ndjson"]
        .iter()
        .any(|candidate| extension.eq_ignore_ascii_case(candidate))
    {
        return Ok(None);
    }
    if parameters.len() > MAX_QUERY_PARAMETERS {
        return Err(invalid("JSON query parameters exceed 64 KiB"));
    }
    let fields = parameters
        .split('&')
        .filter_map(|field| field.split_once('='))
        .collect::<Vec<_>>();
    let Some((_, encoded_query)) = fields.iter().find(|(key, _)| matches!(*key, "q" | "query"))
    else {
        return Ok(None);
    };
    if encoded_query.is_empty() {
        return Ok(None);
    }
    let query = decode_query(encoded_query);
    if query.len() > MAX_QUERY_BYTES {
        return Err(invalid("JSON query filter exceeds 16 KiB"));
    }
    let parameter = |key: &str| {
        fields
            .iter()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| *value)
    };
    let flag = |key: &str| {
        parameter(key).is_some_and(|value| {
            let value = decode_query(value);
            value == "1" || value.eq_ignore_ascii_case("true")
        })
    };
    let count = |key: &str, minimum: usize| {
        parameter(key)
            .and_then(|value| decode_query(value).parse::<usize>().ok())
            .filter(|value| *value >= minimum)
    };
    let offset = count("offset", 0);
    let limit = count("limit", 1);
    let paged = offset.is_some() || limit.is_some();
    Ok(Some(QueryRequest {
        source_path: source_path.to_owned(),
        query,
        raw: flag("raw"),
        compact: flag("compact"),
        offset: paged.then_some(offset.unwrap_or(0)),
        limit: paged.then_some(limit.unwrap_or(100).min(1000)),
    }))
}

fn decode_query(input: &str) -> String {
    let mut output = Vec::with_capacity(input.len());
    let mut bytes = input.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let Some(high) = bytes.next().and_then(hex_digit) else {
                return input.to_owned();
            };
            let Some(low) = bytes.next().and_then(hex_digit) else {
                return input.to_owned();
            };
            output.push((high << 4) | low);
        } else {
            output.push(byte);
        }
    }
    String::from_utf8(output).unwrap_or_else(|_| input.to_owned())
}

const fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

impl QueryRequest {
    fn continuation(&self, offset: usize) -> String {
        let query = self.query.replace('%', "%25").replace('&', "%26");
        format!(
            "?q={query}{}{}&limit={}&offset={offset}",
            if self.raw { "&raw=true" } else { "" },
            if self.compact { "&compact=true" } else { "" },
            self.limit.unwrap_or(100),
        )
    }
}

/// Internal modes are an enum so source text can never select a command/flag.
#[derive(Clone, Copy, Debug)]
pub(super) enum WorkerKind {
    Json,
    Sqlite,
}

impl WorkerKind {
    const fn flag(self) -> &'static str {
        match self {
            Self::Json => "--internal-json-query",
            Self::Sqlite => "--internal-sqlite-query",
        }
    }

    const fn input_limit(self) -> usize {
        match self {
            Self::Json => MAX_JSON_REQUEST,
            Self::Sqlite => MAX_SQLITE_REQUEST,
        }
    }
}

struct WorkerPermit;

impl WorkerPermit {
    fn acquire() -> Result<Self> {
        ACTIVE_WORKERS
            .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < MAX_ACTIVE_WORKERS).then_some(count + 1)
            })
            .map(|_| Self)
            .map_err(|_| {
                invalid("Four structured-data queries are already running; retry when one finishes")
            })
    }
}

impl Drop for WorkerPermit {
    fn drop(&mut self) {
        ACTIVE_WORKERS.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Never search PATH or honor an arbitrary helper environment variable. Cargo
/// test executables live in `target/<profile>/deps`, beside the built Pi binary.
fn worker_candidate(executable: &Path) -> Result<PathBuf> {
    let name = format!("pi{}", std::env::consts::EXE_SUFFIX);
    let parent = executable
        .parent()
        .ok_or_else(|| invalid("Cannot locate the Pi query worker"))?;
    if parent
        .file_name()
        .is_some_and(|directory| directory == "deps")
    {
        return parent
            .parent()
            .map(|profile| profile.join(name))
            .ok_or_else(|| invalid("Cannot locate the Pi binary beside the test executable"));
    }
    if executable
        .file_name()
        .is_some_and(|file| file == name.as_str())
    {
        Ok(executable.to_path_buf())
    } else {
        // Embedders can install Pi beside their application without introducing
        // a shell, PATH lookup, or an untrusted environment override.
        Ok(parent.join(name))
    }
}

struct WorkerGuard {
    child: Option<AgentChild>,
    writer: Option<JoinHandle<io::Result<()>>>,
    stdout: Option<JoinHandle<io::Result<Vec<u8>>>>,
    stderr: Option<JoinHandle<io::Result<Vec<u8>>>>,
}

impl WorkerGuard {
    const fn new(child: AgentChild) -> Self {
        Self {
            child: Some(child),
            writer: None,
            stdout: None,
            stderr: None,
        }
    }
}

impl Drop for WorkerGuard {
    fn drop(&mut self) {
        // A cancelled future may own a blocked stdin writer. Kill and reap
        // before joining any thread so every pipe is guaranteed to settle.
        drop(self.child.take());
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
        if let Some(stdout) = self.stdout.take() {
            let _ = stdout.join();
        }
        if let Some(stderr) = self.stderr.take() {
            let _ = stderr.join();
        }
    }
}

fn join_pipe<T>(handle: Option<JoinHandle<io::Result<T>>>, name: &str) -> Result<T> {
    handle
        .ok_or_else(|| invalid(format!("Missing query worker {name} owner")))?
        .join()
        .map_err(|_| invalid(format!("Query worker {name} reader panicked")))?
        .map_err(Error::from)
}

fn capture_stdout(mut pipe: impl io::Read, exceeded: &AtomicBool) -> io::Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let count = pipe.read(&mut buffer)?;
        if count == 0 {
            return Ok(output);
        }
        let admitted = count.min(MAX_RESPONSE_BYTES.saturating_sub(output.len()));
        output.extend_from_slice(&buffer[..admitted]);
        if admitted != count {
            exceeded.store(true, Ordering::Release);
        }
    }
}

fn capture_stderr(mut pipe: impl io::Read) -> io::Result<Vec<u8>> {
    let mut tail = DiagnosticTail::default();
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let count = pipe.read(&mut buffer)?;
        if count == 0 {
            return Ok(tail.bytes);
        }
        tail.write_all(&buffer[..count])?;
    }
}

/// Own a complete worker transaction, including input transfer and bounded
/// concurrent pipe draining. Dropping this future synchronously reaps the child.
#[allow(clippy::too_many_lines)]
pub(super) async fn run_worker(kind: WorkerKind, request: Vec<u8>) -> Result<Vec<u8>> {
    if request.len() > kind.input_limit() {
        return Err(invalid(
            "Structured-data worker request exceeds its input limit",
        ));
    }
    let owner = AgentCx::for_current_or_request();
    if !owner.capabilities().io || !owner.capabilities().spawn || !owner.capabilities().time {
        return Err(invalid(
            "Structured-data queries require I/O, subprocess, and timer capabilities",
        ));
    }
    owner.checkpoint().map_err(|_| Error::Aborted)?;
    let _permit = WorkerPermit::acquire()?;
    let started = Instant::now();
    let executable = worker_candidate(&std::env::current_exe()?)?;
    if !executable.is_file() {
        return Err(invalid(format!(
            "The bundled Pi query worker is unavailable at {}; build or install the pi binary alongside the application",
            executable.display(),
        )));
    }
    let mut command = owner.process().command(executable);
    command
        .arg(kind.flag())
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    if let Some(root) = std::env::var_os("SystemRoot") {
        command.env("SystemRoot", root);
    }
    let mut guard = WorkerGuard::new(command.spawn()?);
    let child = guard
        .child
        .as_mut()
        .ok_or_else(|| invalid("Missing query worker child"))?;
    let mut stdin = child
        .take_stdin()
        .ok_or_else(|| invalid("Missing query worker stdin"))?;
    let stdout = child
        .take_stdout()
        .ok_or_else(|| invalid("Missing query worker stdout"))?;
    let stderr = child
        .take_stderr()
        .ok_or_else(|| invalid("Missing query worker stderr"))?;
    let exceeded = Arc::new(AtomicBool::new(false));
    guard.writer = Some(
        std::thread::Builder::new()
            .name("pi-query-input".into())
            .spawn(move || stdin.write_all(&request))?,
    );
    let reader_exceeded = Arc::clone(&exceeded);
    guard.stdout = Some(
        std::thread::Builder::new()
            .name("pi-query-output".into())
            .spawn(move || capture_stdout(stdout, &reader_exceeded))?,
    );
    guard.stderr = Some(
        std::thread::Builder::new()
            .name("pi-query-errors".into())
            .spawn(move || capture_stderr(stderr))?,
    );
    let status = loop {
        owner.checkpoint().map_err(|_| Error::Aborted)?;
        if exceeded.load(Ordering::Acquire) {
            return Err(invalid(
                "Structured-data query worker exceeded the 32 MiB response limit",
            ));
        }
        if started.elapsed() >= WORKER_TIMEOUT {
            return Err(invalid("Structured-data query timed out after 30 seconds"));
        }
        if let Some(status) = guard
            .child
            .as_mut()
            .ok_or_else(|| invalid("Missing query worker child"))?
            .try_wait()?
        {
            break status;
        }
        owner.time().sleep(Duration::from_millis(10)).await;
    };
    let stdout = join_pipe(guard.stdout.take(), "stdout")?;
    let stderr = join_pipe(guard.stderr.take(), "stderr")?;
    if exceeded.load(Ordering::Acquire) {
        return Err(invalid(
            "Structured-data query worker exceeded the 32 MiB response limit",
        ));
    }
    if !status.success() {
        let diagnostics = String::from_utf8_lossy(&stderr);
        return Err(invalid(format!(
            "Structured-data query worker stopped ({status}); the query may have exceeded its memory or stack limit{}{}",
            if diagnostics.trim().is_empty() {
                ""
            } else {
                ": "
            },
            diagnostics.trim(),
        )));
    }
    join_pipe(guard.writer.take(), "stdin")?;
    Ok(stdout)
}

/// Called only in the private worker, before reading its request. Lowering the
/// hard limits prevents native filters from exhausting the agent's address
/// space. An unsupported hard allocation limit fails closed.
pub(super) fn set_worker_limits() -> io::Result<()> {
    #[cfg(all(unix, not(target_os = "openbsd")))]
    {
        use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
        // Older XNU accepted RLIMIT_AS without applying a map-size limit.
        // Darwin 22 (macOS 13) and newer enforce it in the VM map allocator.
        #[cfg(target_vendor = "apple")]
        if !sysinfo::System::kernel_version()
            .and_then(|version| version.split('.').next()?.parse::<u32>().ok())
            .is_some_and(|major| major >= 22)
        {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Native structured-data queries require Darwin 22 / macOS 13 or newer for a hard allocation limit",
            ));
        }
        let lower = |resource, ceiling| {
            let old = getrlimit(resource);
            let limit = old
                .current
                .unwrap_or(ceiling)
                .min(old.maximum.unwrap_or(ceiling))
                .min(ceiling);
            setrlimit(
                resource,
                Rlimit {
                    current: Some(limit),
                    maximum: Some(limit),
                },
            )
            .map_err(io::Error::from)
        };
        lower(Resource::As, MAX_WORKER_MEMORY)?;
        lower(Resource::Cpu, WORKER_TIMEOUT.as_secs())?;
        lower(Resource::Stack, 16 * 1024 * 1024)?;
        lower(Resource::Core, 0)
    }
    #[cfg(not(all(unix, not(target_os = "openbsd"))))]
    {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Native structured-data queries require a hard process allocation limit, which is not available on this platform",
        ))
    }
}

pub(super) async fn execute(selector: QueryRequest, source: Vec<u8>) -> Result<QueryOutcome> {
    if source.len() > MAX_INPUT_BYTES {
        return Err(invalid("JSON query input exceeds 10 MiB"));
    }
    if selector.query.len() > MAX_QUERY_BYTES {
        return Err(invalid("JSON query filter exceeds 16 KiB"));
    }
    let request = WorkerRequest {
        selector,
        source_base64: base64::engine::general_purpose::STANDARD.encode(source),
    };
    let bytes = run_worker(WorkerKind::Json, serde_json::to_vec(&request)?).await?;
    serde_json::from_slice::<std::result::Result<QueryOutcome, String>>(&bytes)?.map_err(invalid)
}

pub(super) fn run_json_worker() -> i32 {
    if let Err(error) = set_worker_limits() {
        eprintln!("Cannot establish JSON query worker limits: {error}");
        return 1;
    }
    let result = read_json_worker_request().and_then(|request| {
        let source = base64::engine::general_purpose::STANDARD
            .decode(request.source_base64)
            .map_err(|error| format!("Invalid JSON worker input encoding: {error}"))?;
        evaluate(&request.selector, &source)
    });
    let mut response = BoundedWriter::new(MAX_RESPONSE_BYTES);
    if let Err(error) = serde_json::to_writer(&mut response, &result) {
        eprintln!("Cannot serialize JSON query response: {error}");
        return 1;
    }
    if let Err(error) = io::stdout().lock().write_all(&response.bytes) {
        eprintln!("Cannot write JSON query response: {error}");
        return 1;
    }
    0
}

fn read_json_worker_request() -> std::result::Result<WorkerRequest, String> {
    let mut input = Vec::new();
    io::stdin()
        .lock()
        .take((MAX_JSON_REQUEST + 1) as u64)
        .read_to_end(&mut input)
        .map_err(|error| format!("Cannot read JSON query request: {error}"))?;
    if input.len() > MAX_JSON_REQUEST {
        return Err("JSON query request exceeds 16 MiB".into());
    }
    serde_json::from_slice(&input).map_err(|error| format!("Invalid JSON query request: {error}"))
}

struct BoundedWriter {
    bytes: Vec<u8>,
    limit: usize,
}

impl BoundedWriter {
    const fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
        }
    }
}

impl io::Write for BoundedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(io::Error::other(
                "JSON query output exceeds its capture limit; narrow the filter or page the results",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Default)]
struct DiagnosticTail {
    bytes: Vec<u8>,
}

impl DiagnosticTail {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).trim().to_owned()
    }
}

impl io::Write for DiagnosticTail {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() >= MAX_DIAGNOSTIC_BYTES {
            self.bytes.clear();
            self.bytes
                .extend_from_slice(&bytes[bytes.len() - MAX_DIAGNOSTIC_BYTES..]);
        } else {
            let discard = self
                .bytes
                .len()
                .saturating_add(bytes.len())
                .saturating_sub(MAX_DIAGNOSTIC_BYTES);
            self.bytes.drain(..discard);
            self.bytes.extend_from_slice(bytes);
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct QueryData;

#[derive(Clone, Copy)]
struct RuntimeData<'a> {
    lut: &'a Lut<QueryData>,
    inputs: jaq_std::input::Inputs<'a, Val>,
    diagnostics: &'a RefCell<DiagnosticTail>,
    source_path: &'a str,
}

impl DataT for QueryData {
    type V<'a> = Val;
    type Data<'a> = RuntimeData<'a>;
}

impl<'a> HasLut<'a, QueryData> for RuntimeData<'a> {
    fn lut(&self) -> &'a Lut<QueryData> {
        self.lut
    }
}

impl<'a> jaq_std::input::HasInputs<'a, Val> for RuntimeData<'a> {
    fn inputs(&self) -> jaq_std::input::Inputs<'a, Val> {
        self.inputs
    }
}

fn query_funs() -> impl Iterator<Item = native::Fun<QueryData>> {
    let replacements: Box<[native::Filter<RunPtr<QueryData>>]> = Box::new([
        // A Read query is not a credential inspection operation. This also
        // protects explicit worker invocations that bypass the parent's env_clear.
        ("env", v(0), |_| bome(Ok(Val::Obj(Default::default())))),
        ("input_filename", v(0), |cv| {
            bome(Ok(Val::utf8_str(cv.0.data().source_path.to_owned())))
        }),
        ("debug_empty", v(0), |cv| {
            let mut diagnostics = cv.0.data().diagnostics.borrow_mut();
            let _ = writeln!(diagnostics, "[\"DEBUG\",{}]", cv.1);
            Box::new(std::iter::empty())
        }),
        ("stderr_empty", v(0), |cv| {
            let mut diagnostics = cv.0.data().diagnostics.borrow_mut();
            if let Some(bytes) = cv.1.as_utf8_bytes() {
                let _ = diagnostics.write_all(bytes);
                let _ = diagnostics.write_all(b"\n");
            } else {
                let _ = writeln!(diagnostics, "{}", cv.1);
            }
            Box::new(std::iter::empty())
        }),
        ("@csv", v(0), |cv| bome(tabular_value(&cv.1, false))),
        ("tocsv", v(0), |cv| bome(tabular_value(&cv.1, false))),
        ("@tsv", v(0), |cv| bome(tabular_value(&cv.1, true))),
        ("totsv", v(0), |cv| bome(tabular_value(&cv.1, true))),
    ]);
    jaq_core::funs()
        .chain(
            jaq_std::funs()
                .filter(|(name, _, _)| !matches!(*name, "env" | "debug_empty" | "stderr_empty")),
        )
        .chain(jaq_json::funs())
        .chain(
            jaq_std::input::funs()
                .into_vec()
                .into_iter()
                .map(native::run),
        )
        .chain(replacements.into_vec().into_iter().map(native::run))
}

fn tabular_value(value: &Val, tsv: bool) -> jaq_json::ValR {
    jaq_fmts::write::tabular::Row::try_from(value)
        .map_err(|error| jaq_json::Error::str(format_args!("Cannot format row: {error}")))
        .and_then(|row| {
            let mut output = BoundedWriter::new(MAX_OUTPUT_BYTES);
            let result = if tsv {
                row.write_tsv(&mut output)
            } else {
                row.write_csv(&mut output)
            };
            result.map_err(jaq_json::Error::str)?;
            Ok(Val::utf8_str(output.bytes))
        })
}

fn check_source_depth(source: &[u8]) -> std::result::Result<(), String> {
    let mut depth = 0_usize;
    let mut string = false;
    let mut escape = false;
    let mut comment = false;
    for byte in source {
        if comment {
            comment = *byte != b'\n';
        } else if string {
            if escape {
                escape = false;
            } else if *byte == b'\\' {
                escape = true;
            } else if *byte == b'"' {
                string = false;
            }
        } else {
            match byte {
                b'"' => string = true,
                b'#' => comment = true,
                b'[' | b'{' => {
                    depth += 1;
                    if depth > MAX_JSON_DEPTH {
                        return Err("JSON query input exceeds 256 levels of nesting".into());
                    }
                }
                b']' | b'}' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
    }
    Ok(())
}

fn bounded_error(prefix: &str, error: &impl std::fmt::Debug) -> String {
    let mut diagnostics = DiagnosticTail::default();
    let _ = write!(diagnostics, "{error:?}");
    format!("{prefix}: {}", diagnostics.text())
}

struct QueryOutput<'a> {
    selector: &'a QueryRequest,
    output: BoundedWriter,
    first: Option<Val>,
    seen: usize,
    returned: usize,
    has_more: bool,
    remaining: Option<usize>,
}

impl<'a> QueryOutput<'a> {
    const fn new(selector: &'a QueryRequest) -> Self {
        Self {
            selector,
            output: BoundedWriter::new(MAX_OUTPUT_BYTES),
            first: None,
            seen: 0,
            returned: 0,
            has_more: false,
            remaining: None,
        }
    }

    fn write_value(&mut self, value: &Val) -> std::result::Result<(), String> {
        if self.returned > 0 {
            self.output
                .write_all(b"\n")
                .map_err(|error| error.to_string())?;
        }
        let result = if self.selector.raw {
            if let Some(bytes) = value.as_utf8_bytes() {
                self.output.write_all(bytes)
            } else {
                self.write_json(value)
            }
        } else {
            self.write_json(value)
        };
        result.map_err(|error| format!("JSON query output exceeds 5 MiB: {error}"))?;
        self.returned += 1;
        Ok(())
    }

    fn write_json(&mut self, value: &Val) -> io::Result<()> {
        let pp = jaq_json::write::Pp {
            indent: (!self.selector.compact).then(|| "  ".to_owned()),
            sep_space: !self.selector.compact,
            ..Default::default()
        };
        jaq_json::write::write(&mut self.output, &pp, 0, value)
    }

    /// Return false after one complete lookahead result proves a page is full.
    fn accept(&mut self, value: Val) -> std::result::Result<bool, String> {
        let index = self.seen;
        self.seen = self
            .seen
            .checked_add(1)
            .ok_or_else(|| "JSON result count overflow".to_owned())?;
        if self.selector.limit.is_none() {
            self.write_value(&value)?;
            return Ok(true);
        }
        if index == 0 {
            // Only a single array result pages its members. A stream containing
            // arrays pages whole values; retaining one result distinguishes them.
            self.first = Some(value);
            return Ok(true);
        }
        if let Some(first) = self.first.take()
            && !self.accept_stream(&first, 0)?
        {
            return Ok(false);
        }
        self.accept_stream(&value, index)
    }

    fn accept_stream(&mut self, value: &Val, index: usize) -> std::result::Result<bool, String> {
        if index < self.selector.offset.unwrap_or(0) {
            return Ok(true);
        }
        if self.returned >= self.selector.limit.unwrap_or(100) {
            self.has_more = true;
            return Ok(false);
        }
        self.write_value(value)?;
        Ok(true)
    }

    fn finish(mut self) -> std::result::Result<RenderedOutput, String> {
        if let Some(first) = self.first.take() {
            if let Val::Arr(values) = first {
                let offset = self.selector.offset.unwrap_or(0);
                let count = values
                    .len()
                    .saturating_sub(offset)
                    .min(self.selector.limit.unwrap_or(100));
                let page = Val::Arr(
                    values
                        .iter()
                        .skip(offset)
                        .take(count)
                        .cloned()
                        .collect::<Vec<_>>()
                        .into(),
                );
                self.write_json(&page)
                    .map_err(|error| format!("JSON array page exceeds 5 MiB: {error}"))?;
                self.returned = count;
                let remaining = values.len().saturating_sub(offset).saturating_sub(count);
                self.has_more = remaining > 0;
                self.remaining = Some(remaining);
            } else {
                self.accept_stream(&first, 0)?;
            }
        }
        let mut content = String::from_utf8_lossy(&self.output.bytes).into_owned();
        let next_offset = self.has_more.then(|| {
            self.selector
                .offset
                .unwrap_or(0)
                .saturating_add(self.returned)
        });
        if let Some(offset) = next_offset {
            let count = self.remaining.map_or_else(
                || "more results".to_owned(),
                |count| format!("{count} more items"),
            );
            if !content.is_empty() {
                content.push('\n');
            }
            content.push_str(&format!(
                "[{count}; append {} to continue]",
                self.selector.continuation(offset)
            ));
        } else if self.returned == 0
            && self.remaining.is_none()
            && self.selector.offset.is_some_and(|offset| offset > 0)
        {
            content.push_str(&format!(
                "[offset {} is past the last result ({} total)]",
                self.selector.offset.unwrap_or(0),
                self.seen
            ));
        }
        Ok(RenderedOutput {
            content,
            returned: self.returned,
            seen: self.seen,
            has_more: self.has_more,
            remaining: self.remaining,
            next_offset,
        })
    }
}

struct RenderedOutput {
    content: String,
    returned: usize,
    seen: usize,
    has_more: bool,
    remaining: Option<usize>,
    next_offset: Option<usize>,
}

/// Synchronous by design: called only after the child has established limits.
/// Unit tests exercise finite fixtures directly; production always uses execute.
#[allow(clippy::too_many_lines)]
fn evaluate(selector: &QueryRequest, source: &[u8]) -> std::result::Result<QueryOutcome, String> {
    if source.len() > MAX_INPUT_BYTES {
        return Err("JSON query input exceeds 10 MiB".into());
    }
    if selector.query.len() > MAX_QUERY_BYTES {
        return Err("JSON query filter exceeds 16 KiB".into());
    }
    if selector
        .limit
        .is_some_and(|limit| limit == 0 || limit > 1000)
        || selector.offset.is_some() != selector.limit.is_some()
    {
        return Err(
            "Invalid JSON query pagination; use an offset and a limit from 1 through 1000".into(),
        );
    }
    std::str::from_utf8(source).map_err(|error| format!("JSON input is not UTF-8: {error}"))?;
    check_source_depth(source)?;
    let defs = jaq_core::defs()
        .chain(jaq_std::defs())
        .chain(jaq_json::defs());
    // Loader::new refuses module I/O. Data imports use a separate callback and
    // must be refused explicitly before the compiler assumes imported values.
    let loader = Loader::new(defs);
    let arena = Arena::default();
    let modules = loader
        .load(
            &arena,
            File {
                code: selector.query.as_str(),
                path: (),
            },
        )
        .map_err(|errors| bounded_error("Failed to parse JSON query", &errors))?;
    jaq_core::load::import(&modules, |_| {
        Err("JSON Read queries cannot import files".to_owned())
    })
    .map_err(|errors| bounded_error("Failed to load JSON query", &errors))?;
    let filter = jaq_core::Compiler::default()
        .with_funs(query_funs())
        .with_global_vars(["$ENV", "$ARGS"])
        .compile(modules)
        .map_err(|errors| bounded_error("Failed to compile JSON query", &errors))?;
    // Stop after the first malformed input instead of repeatedly advancing a
    // parser that may not have consumed the failing token. Runtime filter errors
    // are different: they report the failed input and continue with later ones.
    let parsed = jaq_json::read::parse_many(source).scan(false, |failed, item| {
        if *failed {
            None
        } else {
            *failed = item.is_err();
            Some(item.map_err(|error| format!("Invalid JSON input at {error}")))
        }
    });
    let inputs = jaq_std::input::RcIter::new(parsed);
    let diagnostics = RefCell::new(DiagnosticTail::default());
    let data = RuntimeData {
        lut: &filter.lut,
        inputs: &inputs,
        diagnostics: &diagnostics,
        source_path: &selector.source_path,
    };
    let args = Val::obj(
        [
            (Val::utf8_str("positional"), Val::Arr(Default::default())),
            (Val::utf8_str("named"), Val::Obj(Default::default())),
        ]
        .into_iter()
        .collect(),
    );
    let mut output = QueryOutput::new(selector);
    let mut last_input_failed = false;
    let mut runtime_errors = 0_usize;
    'inputs: for input in &inputs {
        let input = input?;
        last_input_failed = false;
        let context = Ctx::<QueryData>::new(
            data,
            Vars::new([Val::Obj(Default::default()), args.clone()]),
        );
        for item in filter.id.run((context, input)) {
            match item {
                Ok(value) => {
                    if !output.accept(value)? {
                        break 'inputs;
                    }
                }
                Err(exception) => match exception.get_halt() {
                    Ok(code) => {
                        last_input_failed = code != 0;
                        if code != 0 {
                            runtime_errors += 1;
                            let _ = writeln!(
                                diagnostics.borrow_mut(),
                                "JSON query halted with code {code}"
                            );
                        }
                        break 'inputs;
                    }
                    Err(exception) => {
                        last_input_failed = true;
                        runtime_errors += 1;
                        if let Ok(error) = exception.get_err() {
                            let _ = writeln!(diagnostics.borrow_mut(), "{error}");
                        } else {
                            let _ = writeln!(
                                diagnostics.borrow_mut(),
                                "Unexpected JSON query control flow"
                            );
                        }
                        break;
                    }
                },
            }
        }
    }
    let rendered = output.finish()?;
    let diagnostic_text = diagnostics.borrow().text();
    let content = if diagnostic_text.is_empty() {
        rendered.content
    } else if rendered.content.is_empty() {
        format!("[jq stderr: {diagnostic_text}]")
    } else {
        format!("[jq stderr: {diagnostic_text}]\n{}", rendered.content)
    };
    let next_path = rendered
        .next_offset
        .map(|offset| format!("{}{}", selector.source_path, selector.continuation(offset)));
    Ok(QueryOutcome {
        content,
        is_error: last_input_failed,
        metadata: serde_json::json!({
            "format": "json",
            "engine": "jaq",
            "engineVersion": "3.1.1",
            "sourcePath": selector.source_path,
            "query": selector.query,
            "raw": selector.raw,
            "compact": selector.compact,
            "immutable": true,
            "runtimeErrors": runtime_errors,
            "diagnostics": diagnostic_text,
            "returned": rendered.returned,
            "observedResults": rendered.seen,
            "pagination": selector.limit.map(|limit| serde_json::json!({
                "offset": selector.offset.unwrap_or(0),
                "limit": limit,
                "hasMore": rendered.has_more,
                "remaining": rendered.remaining,
                "nextOffset": rendered.next_offset,
                "nextPath": next_path,
            })),
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(query: &str) -> QueryRequest {
        QueryRequest {
            source_path: "fixture.json".into(),
            query: query.into(),
            raw: false,
            compact: true,
            offset: None,
            limit: None,
        }
    }

    fn query(filter: &str, source: &str) -> QueryOutcome {
        evaluate(&request(filter), source.as_bytes()).expect("finite query fixture")
    }

    #[test]
    fn selectors_preserve_arithmetic_percent_and_optional_operators() {
        let arithmetic = parse_path("local://data.JSON?q=.items[]? | .n + 1&raw=true")
            .unwrap()
            .unwrap();
        assert_eq!(arithmetic.source_path, "local://data.JSON");
        assert_eq!(arithmetic.query, ".items[]? | .n + 1");
        assert!(arithmetic.raw);
        assert_eq!(arithmetic.limit, None);
        let modulo = parse_path("numbers.json?q=. % 3").unwrap().unwrap();
        assert_eq!(modulo.query, ". % 3");
        let decoded = parse_path("events.ndjson?query=.name%20%2B%20%22a%26b%22&compact=1")
            .unwrap()
            .unwrap();
        assert_eq!(decoded.query, ".name + \"a&b\"");
        assert!(decoded.compact);
    }

    #[test]
    fn ordinary_paths_remain_ordinary_reads_and_pages_are_opt_in() {
        for path in [
            "data.json",
            "file.txt?q=.",
            "data.json?raw=true",
            "data.json?q=",
        ] {
            assert!(parse_path(path).unwrap().is_none(), "{path}");
        }
        let offset = parse_path("data.json?q=.&offset=0").unwrap().unwrap();
        assert_eq!((offset.offset, offset.limit), (Some(0), Some(100)));
        let capped = parse_path("data.json?q=.&limit=1000000").unwrap().unwrap();
        assert_eq!((capped.offset, capped.limit), (Some(0), Some(1000)));
        assert!(parse_path(&format!("data.json?q={}", "x".repeat(MAX_QUERY_BYTES + 1))).is_err());
    }

    #[test]
    fn full_filters_support_arithmetic_projection_grouping_and_definitions() {
        let source = r#"[{"kind":"a","amount":2},{"kind":"b","amount":7},{"kind":"a","amount":3}]"#;
        let grouped = query(
            "group_by(.kind) | map({kind: .[0].kind, total: (map(.amount) | add)})",
            source,
        );
        assert_eq!(
            grouped.content,
            r#"[{"kind":"a","total":5},{"kind":"b","total":7}]"#
        );
        let projected = query(
            ".[] | select(.amount >= 3) | {label: (.kind + \"!\"), value: (.amount * 2)}",
            source,
        );
        assert_eq!(
            projected.content,
            "{\"label\":\"b!\",\"value\":14}\n{\"label\":\"a!\",\"value\":6}"
        );
        assert_eq!(
            query("def scale($n): map(. * $n); scale(3)", "[1,2,3]").content,
            "[3,6,9]"
        );
        assert_eq!(
            query("reduce .[] as $n (0; . + $n)", "[1,2,3]").content,
            "6"
        );
        assert_eq!(
            query("(.a += 2) | if .a > 3 then .a else 0 end", "{\"a\":2}").content,
            "4"
        );
    }

    #[test]
    fn native_formats_and_regular_expressions_are_available() {
        let mut selector = request("[.name, .count] | @csv");
        selector.raw = true;
        assert_eq!(
            evaluate(&selector, br#"{"name":"a,b","count":2}"#)
                .unwrap()
                .content,
            "\"a,b\",2"
        );
        selector.query = "[.name, .count] | @tsv".into();
        assert_eq!(
            evaluate(&selector, br#"{"name":"a\tb","count":2}"#)
                .unwrap()
                .content,
            "a\\tb\t2"
        );
        assert_eq!(
            query("map(select(test(\"^a\")))", r#"["alpha","beta","atom"]"#).content,
            r#"["alpha","atom"]"#
        );
        assert_eq!(query("\"a b\" | @uri", "null").content, "\"a%20b\"");
    }

    #[test]
    fn exact_integer_and_decimal_lexemes_survive_paging() {
        let mut selector = request(".ids");
        selector.offset = Some(0);
        selector.limit = Some(2);
        let output = evaluate(&selector, br#"{"ids":[12345678901234567890,1.0,3]}"#).unwrap();
        assert_eq!(
            output.content,
            "[12345678901234567890,1.0]\n[1 more items; append ?q=.ids&compact=true&limit=2&offset=2 to continue]"
        );
        assert_eq!(output.metadata["pagination"]["remaining"], 1);
        assert_eq!(output.metadata["returned"], 2);
    }

    #[test]
    fn raw_output_preserves_trailing_spaces_and_embedded_newlines() {
        let mut selector = request("\"first\\nsecond  \"");
        selector.raw = true;
        assert_eq!(
            evaluate(&selector, b"null").unwrap().content,
            "first\nsecond  "
        );
        selector.offset = Some(0);
        selector.limit = Some(1);
        assert_eq!(
            evaluate(&selector, b"null").unwrap().content,
            "first\nsecond  "
        );
    }

    #[test]
    fn single_objects_remain_complete_under_pagination() {
        let mut selector = request(".");
        selector.compact = false;
        selector.offset = Some(0);
        selector.limit = Some(1);
        let output = evaluate(&selector, br#"{"nested":{"a":1},"other":[2,3]}"#).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&output.content).unwrap();
        assert_eq!(parsed["nested"]["a"], 1);
        assert_eq!(parsed["other"], serde_json::json!([2, 3]));
        assert_eq!(output.metadata["pagination"]["hasMore"], false);
    }

    #[test]
    fn streams_page_whole_values_including_arrays() {
        let mut selector = request(".");
        selector.offset = Some(1);
        selector.limit = Some(1);
        let output = evaluate(&selector, b"[1,2]\n[3,4]\n[5,6]\n").unwrap();
        assert_eq!(
            output.content,
            "[3,4]\n[more results; append ?q=.&compact=true&limit=1&offset=2 to continue]"
        );
        assert_eq!(output.metadata["returned"], 1);
        assert_eq!(
            output.metadata["pagination"]["remaining"],
            serde_json::Value::Null
        );
    }

    #[test]
    fn full_page_stops_a_large_generator_after_one_lookahead() {
        let mut selector = request("range(0; 1000000000)");
        selector.offset = Some(3);
        selector.limit = Some(2);
        let output = evaluate(&selector, b"null").unwrap();
        assert!(output.content.starts_with("3\n4\n[more results;"));
        assert_eq!(output.metadata["observedResults"], 6);
        assert_eq!(output.metadata["pagination"]["nextOffset"], 5);
    }

    #[test]
    fn continuation_round_trips_filters_and_output_flags() {
        let mut selector = request(r#"["a&b%", "next"]"#);
        selector.raw = true;
        selector.offset = Some(0);
        selector.limit = Some(1);
        let output = evaluate(&selector, b"null").unwrap();
        let next = output.metadata["pagination"]["nextPath"].as_str().unwrap();
        let reparsed = parse_path(next).unwrap().unwrap();
        assert_eq!(reparsed.query, selector.query);
        assert!(reparsed.raw && reparsed.compact);
        assert_eq!((reparsed.offset, reparsed.limit), (Some(1), Some(1)));
        assert_eq!(evaluate(&reparsed, b"null").unwrap().content, "[\"next\"]");
    }

    #[test]
    fn inputs_consumes_the_shared_jsonl_stream_once() {
        assert_eq!(
            query("(., inputs) | .n", "{\"n\":1}\n{\"n\":2}\n{\"n\":3}\n").content,
            "1\n2\n3"
        );
        assert_eq!(
            query(
                "[., inputs] | map(.n) | add",
                "{\"n\":1}\n{\"n\":2}\n{\"n\":3}\n"
            )
            .content,
            "6"
        );
    }

    #[test]
    fn runtime_errors_report_failed_inputs_and_continue_later_records() {
        let recovered = query(".n + 1", "{\"n\":1}\n{\"n\":\"x\"}\n{\"n\":3}\n");
        assert!(recovered.content.starts_with("[jq stderr: "));
        assert!(recovered.content.ends_with("\n2\n4"));
        assert!(!recovered.is_error);
        assert_eq!(recovered.metadata["runtimeErrors"], 1);
        let failed = query(".n + 1", "{\"n\":1}\n{\"n\":\"x\"}\n");
        assert!(failed.is_error);
        assert!(failed.content.ends_with("\n2"));
    }

    #[test]
    fn diagnostics_are_separate_from_results_and_environment_is_empty() {
        let output = query("debug | .name", r#"{"name":"ok"}"#);
        assert!(output.content.contains("DEBUG"));
        assert!(output.content.ends_with("\n\"ok\""));
        assert!(!output.is_error);
        assert_eq!(
            query("[env, $ENV, $ARGS.named]", "null").content,
            "[{},{},{}]"
        );
        assert_eq!(query("input_filename", "null").content, "\"fixture.json\"");
    }

    #[test]
    fn halt_and_caught_errors_keep_their_language_semantics() {
        let halted = query("1, halt, 2", "null");
        assert_eq!(halted.content, "1");
        assert!(!halted.is_error);
        let failed = query("\"explanation\" | halt_error(7)", "null");
        assert!(failed.is_error);
        assert!(failed.content.contains("explanation"));
        assert_eq!(
            query("try error(\"boom\") catch .", "null").content,
            "\"boom\""
        );
    }

    #[test]
    fn invalid_input_filters_and_external_imports_fail_descriptively() {
        assert!(
            evaluate(&request("invalid [[["), b"null")
                .unwrap_err()
                .contains("parse JSON query")
        );
        assert!(
            evaluate(&request("missing_function"), b"null")
                .unwrap_err()
                .contains("compile JSON query")
        );
        assert!(
            evaluate(&request("."), b"{\"broken\":]")
                .unwrap_err()
                .contains("Invalid JSON input")
        );
        let module = evaluate(&request("include \"outside\"; ."), b"null").unwrap_err();
        assert!(module.contains("module loading not supported"));
        let data =
            evaluate(&request("import \"outside\" as $secret; $secret"), b"null").unwrap_err();
        assert!(data.contains("cannot import files"));
    }

    #[test]
    fn parser_and_output_bounds_fail_before_unbounded_capture() {
        let nested = format!(
            "{}null{}",
            "[".repeat(MAX_JSON_DEPTH + 1),
            "]".repeat(MAX_JSON_DEPTH + 1)
        );
        assert!(
            evaluate(&request("."), nested.as_bytes())
                .unwrap_err()
                .contains("256 levels")
        );
        let oversized = vec![b' '; MAX_INPUT_BYTES + 1];
        assert!(
            evaluate(&request("."), &oversized)
                .unwrap_err()
                .contains("10 MiB")
        );
        let large_string = format!("\"{}\"", "x".repeat(3 * 1024 * 1024));
        assert!(
            evaluate(&request(". + ."), large_string.as_bytes())
                .unwrap_err()
                .contains("5 MiB")
        );
        assert!(check_source_depth(br#"{"text":"[[[[[\\\"{{{{"}"#).is_ok());
    }

    #[test]
    fn byte_buffers_enforce_exact_caps_and_keep_error_tails() {
        let mut output = BoundedWriter::new(7);
        output.write_all(b"1234").unwrap();
        assert!(output.write_all(b"5678").is_err());
        assert_eq!(output.bytes, b"1234");
        let mut diagnostics = DiagnosticTail::default();
        diagnostics
            .write_all(&vec![b'x'; MAX_DIAGNOSTIC_BYTES * 2])
            .unwrap();
        diagnostics.write_all(b"last error").unwrap();
        assert_eq!(diagnostics.bytes.len(), MAX_DIAGNOSTIC_BYTES);
        assert!(diagnostics.text().ends_with("last error"));
    }

    #[cfg(unix)]
    #[test]
    fn worker_resolution_never_reexecutes_a_test_harness_or_searches_path() {
        assert_eq!(
            worker_candidate(Path::new("/app/bin/pi")).unwrap(),
            Path::new("/app/bin/pi")
        );
        assert_eq!(
            worker_candidate(Path::new("/build/target/debug/deps/read_tool-01234567")).unwrap(),
            Path::new("/build/target/debug/pi")
        );
        assert_eq!(
            worker_candidate(Path::new("/app/bin/embedder")).unwrap(),
            Path::new("/app/bin/pi")
        );
    }

    #[test]
    fn worker_requires_the_captured_capabilities_before_process_lookup() {
        let restricted = asupersync::Cx::for_request().restrict::<asupersync::cx::cap::None>();
        let _guard = restricted.set_current_restricted();
        let error =
            futures::executor::block_on(run_worker(WorkerKind::Json, Vec::new())).unwrap_err();
        assert!(error.to_string().contains("capabilities"));
    }
}
