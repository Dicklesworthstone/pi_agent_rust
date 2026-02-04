//! Verbose test logging infrastructure.
//!
//! Provides detailed logging for integration and E2E tests to enable
//! easy debugging when tests fail. All log entries capture:
//! - Timestamps (elapsed time from test start)
//! - Log level (Debug, Info, Warn, Error)
//! - Category (setup, action, verify, etc.)
//! - Message with optional key-value context
//!
//! # Example
//!
//! ```ignore
//! let logger = TestLogger::new();
//! logger.info("setup", "Creating test file");
//! logger.with_context(LogLevel::Info, "action", "Calling tool", |ctx| {
//!     ctx.push(("tool".into(), "read".into()));
//!     ctx.push(("path".into(), "/tmp/test.txt".into()));
//! });
//!
//! // On test failure, logs are automatically dumped:
//! // [   0.001s] INFO  [setup]  Creating test file
//! // [   0.002s] INFO  [action] Calling tool
//! //            tool = read
//! //            path = /tmp/test.txt
//! ```

#![allow(dead_code)]

use chrono::{DateTime, SecondsFormat, Utc};
use regex::Regex;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::io::Read as _;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

const REDACTED_VALUE: &str = "[REDACTED]";
const REDACTION_KEYS: [&str; 10] = [
    "api_key",
    "api-key",
    "authorization",
    "bearer",
    "cookie",
    "credential",
    "password",
    "private_key",
    "secret",
    "token",
];

const TEST_LOG_SCHEMA: &str = "pi.test.log.v1";
const TEST_ARTIFACT_SCHEMA: &str = "pi.test.artifact.v1";
const PLACEHOLDER_TIMESTAMP: &str = "<TIMESTAMP>";
const PLACEHOLDER_PROJECT_ROOT: &str = "<PROJECT_ROOT>";
const PLACEHOLDER_TEST_ROOT: &str = "<TEST_ROOT>";
const PLACEHOLDER_RUN_ID: &str = "<RUN_ID>";
const PLACEHOLDER_UUID: &str = "<UUID>";
const PLACEHOLDER_PORT: &str = "<PORT>";

static ANSI_REGEX: OnceLock<Regex> = OnceLock::new();
static RUN_ID_REGEX: OnceLock<Regex> = OnceLock::new();
static UUID_REGEX: OnceLock<Regex> = OnceLock::new();
static LOCAL_PORT_REGEX: OnceLock<Regex> = OnceLock::new();

fn ansi_regex() -> &'static Regex {
    ANSI_REGEX.get_or_init(|| Regex::new(r"\x1b\[[0-9;]*[A-Za-z]").expect("ansi regex"))
}

fn run_id_regex() -> &'static Regex {
    RUN_ID_REGEX.get_or_init(|| Regex::new(r"\brun-[0-9a-fA-F-]{36}\b").expect("run id regex"))
}

fn uuid_regex() -> &'static Regex {
    UUID_REGEX.get_or_init(|| {
        Regex::new(
            r"\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b",
        )
        .expect("uuid regex")
    })
}

fn local_port_regex() -> &'static Regex {
    LOCAL_PORT_REGEX.get_or_init(|| Regex::new(r"http://127\\.0\\.0\\.1:\\d+").expect("port regex"))
}

/// Log entry severity level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    /// Detailed debugging information.
    Debug,
    /// General information about test progress.
    Info,
    /// Warnings about unexpected but non-fatal conditions.
    Warn,
    /// Errors that may cause test failure.
    Error,
}

impl LogLevel {
    /// Returns the display string for this log level.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Debug => "DEBUG",
            Self::Info => "INFO ",
            Self::Warn => "WARN ",
            Self::Error => "ERROR",
        }
    }

    /// Returns the ANSI color code for this log level.
    pub const fn color_code(self) -> &'static str {
        match self {
            Self::Debug => "\x1b[90m", // Gray
            Self::Info => "\x1b[32m",  // Green
            Self::Warn => "\x1b[33m",  // Yellow
            Self::Error => "\x1b[31m", // Red
        }
    }

    pub const fn as_json_str(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

/// A single log entry with timestamp, level, category, message, and context.
#[derive(Debug, Clone)]
pub struct LogEntry {
    /// Elapsed milliseconds from logger creation.
    pub elapsed_ms: u64,
    /// Severity level.
    pub level: LogLevel,
    /// Category tag (e.g., "setup", "action", "verify").
    pub category: String,
    /// Human-readable message.
    pub message: String,
    /// Optional key-value context pairs.
    pub context: Vec<(String, String)>,
}

impl LogEntry {
    /// Format this entry as a string (without colors).
    pub fn format(&self) -> String {
        let elapsed = format_elapsed_ms(self.elapsed_ms);
        let mut output = format!(
            "[{elapsed}s] {} [{}] {}\n",
            self.level.as_str(),
            self.category,
            self.message
        );

        for (key, value) in &self.context {
            let _ = writeln!(output, "           {key} = {value}");
        }

        output
    }

    /// Format this entry with ANSI colors.
    pub fn format_colored(&self) -> String {
        const RESET: &str = "\x1b[0m";
        const DIM: &str = "\x1b[2m";

        let elapsed = format_elapsed_ms(self.elapsed_ms);
        let mut output = format!(
            "{DIM}[{elapsed}s]{RESET} {}{}{RESET} {DIM}[{}]{RESET} {}\n",
            self.level.color_code(),
            self.level.as_str(),
            self.category,
            self.message
        );

        for (key, value) in &self.context {
            let _ = writeln!(output, "{DIM}           {key}{RESET} = {value}");
        }

        output
    }
}

/// Artifact entry captured during a test run.
#[derive(Debug, Clone)]
pub struct ArtifactEntry {
    /// Elapsed milliseconds from logger creation.
    pub elapsed_ms: u64,
    /// Logical name of the artifact.
    pub name: String,
    /// Path to the artifact on disk.
    pub path: String,
}

impl ArtifactEntry {
    /// Format this artifact entry as a string.
    pub fn format(&self) -> String {
        let elapsed = format_elapsed_ms(self.elapsed_ms);
        format!("[{elapsed}s] {} -> {}\n", self.name, self.path)
    }
}

fn format_elapsed_ms(elapsed_ms: u64) -> String {
    let secs = elapsed_ms / 1000;
    let millis = elapsed_ms % 1000;
    let raw = format!("{secs}.{millis:03}");
    format!("{raw:>8}")
}

#[derive(Debug, Clone, Serialize)]
struct TestLogJsonRecord {
    schema: &'static str,
    #[serde(rename = "type")]
    record_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    test: Option<String>,
    seq: usize,
    ts: String,
    t_ms: u64,
    level: &'static str,
    category: String,
    message: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    context: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize)]
struct TestArtifactJsonRecord {
    schema: &'static str,
    #[serde(rename = "type")]
    record_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    test: Option<String>,
    seq: usize,
    ts: String,
    t_ms: u64,
    name: String,
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    size_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sha256: Option<String>,
}

#[derive(Debug, Clone)]
struct NormalizationContext {
    project_root: String,
    test_root: Option<String>,
}

impl NormalizationContext {
    fn new(test_root: Option<&Path>) -> Self {
        let project_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .canonicalize()
            .unwrap_or_else(|_| Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf())
            .display()
            .to_string();
        let test_root = test_root.map(|root| {
            root.canonicalize()
                .unwrap_or_else(|_| root.to_path_buf())
                .display()
                .to_string()
        });
        Self {
            project_root,
            test_root,
        }
    }

    fn normalize_string(&self, input: &str) -> String {
        let without_ansi = ansi_regex().replace_all(input, "");
        let mut out =
            replace_path_variants(&without_ansi, &self.project_root, PLACEHOLDER_PROJECT_ROOT);
        if let Some(test_root) = &self.test_root {
            out = replace_path_variants(&out, test_root, PLACEHOLDER_TEST_ROOT);
        }
        out = run_id_regex()
            .replace_all(&out, PLACEHOLDER_RUN_ID)
            .into_owned();
        out = uuid_regex()
            .replace_all(&out, PLACEHOLDER_UUID)
            .into_owned();
        out = local_port_regex()
            .replace_all(&out, format!("http://127.0.0.1:{PLACEHOLDER_PORT}"))
            .into_owned();
        out
    }
}

fn replace_path_variants(input: &str, path: &str, placeholder: &str) -> String {
    if path.is_empty() {
        return input.to_string();
    }
    let mut out = input.replace(path, placeholder);
    let path_backslashes = path.replace('/', "\\");
    if path_backslashes != path {
        out = out.replace(&path_backslashes, placeholder);
    }
    out
}

/// Thread-safe test logger that captures all log entries.
///
/// Entries are stored in memory and can be dumped on test failure.
/// The logger is designed to have minimal overhead during normal test execution.
pub struct TestLogger {
    /// All captured log entries.
    entries: Mutex<Vec<LogEntry>>,
    /// Captured artifacts produced during the test.
    artifacts: Mutex<Vec<ArtifactEntry>>,
    /// Timestamp when the logger was created.
    start: Instant,
    /// Wall-clock timestamp when the logger was created.
    start_wall: SystemTime,
    /// Minimum log level to capture (entries below this are ignored).
    min_level: LogLevel,
    /// Optional test name for JSONL output.
    test_name: Mutex<Option<String>>,
    /// Optional root path to normalize in JSONL dumps (e.g. harness temp dir).
    normalize_root: Mutex<Option<String>>,
}

impl Default for TestLogger {
    fn default() -> Self {
        Self::new()
    }
}

impl TestLogger {
    /// Create a new test logger with default settings.
    ///
    /// By default, captures all log levels (Debug and above).
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(Vec::with_capacity(256)),
            artifacts: Mutex::new(Vec::with_capacity(16)),
            start: Instant::now(),
            start_wall: SystemTime::now(),
            min_level: LogLevel::Debug,
            test_name: Mutex::new(None),
            normalize_root: Mutex::new(None),
        }
    }

    /// Create a logger that only captures entries at or above the given level.
    #[must_use]
    pub fn with_min_level(min_level: LogLevel) -> Self {
        Self {
            entries: Mutex::new(Vec::with_capacity(256)),
            artifacts: Mutex::new(Vec::with_capacity(16)),
            start: Instant::now(),
            start_wall: SystemTime::now(),
            min_level,
            test_name: Mutex::new(None),
            normalize_root: Mutex::new(None),
        }
    }

    /// Configure a root path for normalization in JSONL dumps.
    ///
    /// This is intended for deterministic, portable artifacts (e.g. CI logs) where
    /// temp directories should not leak into diffs.
    pub fn set_normalization_root(&self, root: impl AsRef<Path>) {
        let root = root.as_ref().display().to_string();
        *self.normalize_root.lock().unwrap() = Some(root);
    }

    /// Set the test name for JSONL output.
    pub fn set_test_name(&self, name: impl Into<String>) {
        *self.test_name.lock().unwrap() = Some(name.into());
    }

    fn elapsed_ms(&self) -> u64 {
        u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// Log an entry with the given level and category.
    pub fn log(&self, level: LogLevel, category: &str, message: impl Into<String>) {
        if (level as u8) < (self.min_level as u8) {
            return;
        }

        let entry = LogEntry {
            elapsed_ms: self.elapsed_ms(),
            level,
            category: category.to_string(),
            message: message.into(),
            context: Vec::new(),
        };

        self.entries.lock().unwrap().push(entry);
    }

    /// Log a debug message.
    pub fn debug(&self, category: &str, message: impl Into<String>) {
        self.log(LogLevel::Debug, category, message);
    }

    /// Log an info message.
    pub fn info(&self, category: &str, message: impl Into<String>) {
        self.log(LogLevel::Info, category, message);
    }

    /// Log a warning message.
    pub fn warn(&self, category: &str, message: impl Into<String>) {
        self.log(LogLevel::Warn, category, message);
    }

    /// Log an error message.
    pub fn error(&self, category: &str, message: impl Into<String>) {
        self.log(LogLevel::Error, category, message);
    }

    /// Log an entry with additional key-value context.
    ///
    /// The closure receives a mutable reference to the context vector,
    /// allowing you to add key-value pairs that will be displayed with the entry.
    ///
    /// # Example
    ///
    /// ```ignore
    /// logger.with_context(LogLevel::Info, "action", "Executing tool", |ctx| {
    ///     ctx.push(("tool".into(), "bash".into()));
    ///     ctx.push(("command".into(), "ls -la".into()));
    /// });
    /// ```
    pub fn with_context<F>(&self, level: LogLevel, category: &str, message: impl Into<String>, f: F)
    where
        F: FnOnce(&mut Vec<(String, String)>),
    {
        if (level as u8) < (self.min_level as u8) {
            return;
        }

        let mut context = Vec::new();
        f(&mut context);
        redact_context(&mut context);

        let entry = LogEntry {
            elapsed_ms: self.elapsed_ms(),
            level,
            category: category.to_string(),
            message: message.into(),
            context,
        };

        self.entries.lock().unwrap().push(entry);
    }

    /// Log an info entry with context.
    pub fn info_ctx<F>(&self, category: &str, message: impl Into<String>, f: F)
    where
        F: FnOnce(&mut Vec<(String, String)>),
    {
        self.with_context(LogLevel::Info, category, message, f);
    }

    /// Log an error entry with context.
    #[allow(dead_code)]
    pub fn error_ctx<F>(&self, category: &str, message: impl Into<String>, f: F)
    where
        F: FnOnce(&mut Vec<(String, String)>),
    {
        self.with_context(LogLevel::Error, category, message, f);
    }

    /// Get the number of logged entries.
    pub fn entry_count(&self) -> usize {
        self.entries.lock().unwrap().len()
    }

    /// Get the elapsed time since logger creation.
    pub fn elapsed(&self) -> std::time::Duration {
        self.start.elapsed()
    }

    /// Dump all log entries as a plain text string.
    pub fn dump(&self) -> String {
        let entries = self.entries.lock().unwrap();
        let mut output = String::with_capacity(entries.len() * 100);

        for entry in entries.iter() {
            output.push_str(&entry.format());
        }

        drop(entries);
        output
    }

    /// Dump all log entries with ANSI color codes.
    pub fn dump_colored(&self) -> String {
        let entries = self.entries.lock().unwrap();
        let mut output = String::with_capacity(entries.len() * 120);

        for entry in entries.iter() {
            output.push_str(&entry.format_colored());
        }

        drop(entries);
        output
    }

    /// Record an artifact produced during the test (e.g. exported files).
    pub fn record_artifact(&self, name: impl Into<String>, path: impl AsRef<Path>) {
        let entry = ArtifactEntry {
            elapsed_ms: self.elapsed_ms(),
            name: name.into(),
            path: path.as_ref().display().to_string(),
        };
        self.artifacts.lock().unwrap().push(entry);
    }

    /// Returns true if any artifacts were recorded.
    pub fn has_artifacts(&self) -> bool {
        !self.artifacts.lock().unwrap().is_empty()
    }

    /// Dump artifact entries as a plain text string.
    pub fn dump_artifacts(&self) -> String {
        let artifacts = self.artifacts.lock().unwrap();
        let mut output = String::with_capacity(artifacts.len() * 80);
        for entry in artifacts.iter() {
            output.push_str(&entry.format());
        }
        drop(artifacts);
        output
    }

    /// Dump logs and artifacts to a file path.
    pub fn write_dump_to_path(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }

        let mut output = self.dump();
        if self.has_artifacts() {
            output.push_str("\n=== ARTIFACTS ===\n");
            output.push_str(&self.dump_artifacts());
            output.push_str("=== END ARTIFACTS ===\n");
        }

        fs::write(path, output)
    }

    /// Dump logs and artifacts as JSONL (one JSON object per line).
    ///
    /// This output is intended for machine parsing and deterministic diffs. It:
    /// - includes a schema tag (`pi.test.log.v1` / `pi.test.artifact.v1`)
    /// - includes sequence numbers + ISO-8601 timestamps
    /// - uses elapsed milliseconds for ordering
    ///
    /// Use `dump_jsonl_normalized()` for deterministic placeholder normalization.
    pub fn dump_jsonl(&self) -> String {
        let normalize_root = self.normalize_root.lock().unwrap().clone();
        let test_name = self.test_name.lock().unwrap().clone();
        self.dump_jsonl_internal(true, false, test_name.as_deref(), normalize_root.as_deref())
    }

    /// Dump normalized log records as JSONL (deterministic placeholders).
    pub fn dump_jsonl_normalized(&self) -> String {
        let normalize_root = self.normalize_root.lock().unwrap().clone();
        let test_name = self.test_name.lock().unwrap().clone();
        self.dump_jsonl_internal(true, true, test_name.as_deref(), normalize_root.as_deref())
    }

    /// Dump only artifact index records as JSONL.
    pub fn dump_artifact_index_jsonl(&self) -> String {
        let normalize_root = self.normalize_root.lock().unwrap().clone();
        let test_name = self.test_name.lock().unwrap().clone();
        self.dump_jsonl_internal(
            false,
            false,
            test_name.as_deref(),
            normalize_root.as_deref(),
        )
    }

    /// Dump normalized artifact index records as JSONL.
    pub fn dump_artifact_index_jsonl_normalized(&self) -> String {
        let normalize_root = self.normalize_root.lock().unwrap().clone();
        let test_name = self.test_name.lock().unwrap().clone();
        self.dump_jsonl_internal(false, true, test_name.as_deref(), normalize_root.as_deref())
    }

    fn dump_jsonl_internal(
        &self,
        include_logs: bool,
        normalized: bool,
        test_name: Option<&str>,
        normalize_root: Option<&str>,
    ) -> String {
        let entries = self.entries.lock().unwrap();
        let artifacts = self.artifacts.lock().unwrap();

        let mut out = String::with_capacity((entries.len() + artifacts.len()).saturating_mul(160));
        let ctx = if normalized {
            Some(NormalizationContext::new(normalize_root.map(Path::new)))
        } else {
            None
        };

        let mut seq: usize = 1;
        if include_logs {
            for entry in entries.iter() {
                let record = build_log_record(
                    entry,
                    seq,
                    test_name,
                    ctx.as_ref(),
                    self.start_wall,
                    normalized,
                );
                seq = seq.saturating_add(1);
                let line = serde_json::to_string(&record)
                    .unwrap_or_else(|_| "{\"schema\":\"pi.test.log.v1\"}".to_string());
                out.push_str(&line);
                out.push('\n');
            }
        }

        for artifact in artifacts.iter() {
            let record = build_artifact_record(
                artifact,
                seq,
                test_name,
                ctx.as_ref(),
                self.start_wall,
                normalized,
            );
            seq = seq.saturating_add(1);
            let line = serde_json::to_string(&record)
                .unwrap_or_else(|_| "{\"schema\":\"pi.test.artifact.v1\"}".to_string());
            out.push_str(&line);
            out.push('\n');
        }

        drop(artifacts);
        drop(entries);

        out
    }

    /// Write JSONL dump to a file path.
    pub fn write_jsonl_to_path(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
        write_string_to_path(path.as_ref(), &self.dump_jsonl())
    }

    /// Write normalized JSONL dump to a file path.
    pub fn write_jsonl_normalized_to_path(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
        write_string_to_path(path.as_ref(), &self.dump_jsonl_normalized())
    }

    /// Write artifact index JSONL to a file path.
    pub fn write_artifact_index_jsonl_to_path(
        &self,
        path: impl AsRef<Path>,
    ) -> std::io::Result<()> {
        write_string_to_path(path.as_ref(), &self.dump_artifact_index_jsonl())
    }

    /// Write normalized artifact index JSONL to a file path.
    pub fn write_artifact_index_jsonl_normalized_to_path(
        &self,
        path: impl AsRef<Path>,
    ) -> std::io::Result<()> {
        write_string_to_path(path.as_ref(), &self.dump_artifact_index_jsonl_normalized())
    }

    /// Clear all log entries.
    #[allow(dead_code)]
    pub fn clear(&self) {
        self.entries.lock().unwrap().clear();
        self.artifacts.lock().unwrap().clear();
    }

    /// Get a copy of all entries (useful for assertions).
    pub fn entries(&self) -> Vec<LogEntry> {
        self.entries.lock().unwrap().clone()
    }

    /// Get a copy of all artifacts (useful for assertions).
    pub fn artifacts(&self) -> Vec<ArtifactEntry> {
        self.artifacts.lock().unwrap().clone()
    }

    /// Check if any error-level entries were logged.
    pub fn has_errors(&self) -> bool {
        self.entries
            .lock()
            .unwrap()
            .iter()
            .any(|e| e.level == LogLevel::Error)
    }

    /// Get all error messages.
    pub fn error_messages(&self) -> Vec<String> {
        self.entries
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.level == LogLevel::Error)
            .map(|e| e.message.clone())
            .collect()
    }
}

fn redact_context(context: &mut [(String, String)]) {
    for (key, value) in context.iter_mut() {
        if is_sensitive_key(key) {
            *value = REDACTED_VALUE.to_string();
        }
    }
}

fn is_sensitive_key(key: &str) -> bool {
    let key = key.trim().to_ascii_lowercase();
    REDACTION_KEYS.iter().any(|needle| key.contains(needle))
}

fn build_log_record(
    entry: &LogEntry,
    seq: usize,
    test_name: Option<&str>,
    ctx: Option<&NormalizationContext>,
    start_wall: SystemTime,
    normalized: bool,
) -> TestLogJsonRecord {
    let (ts, t_ms) = if normalized {
        (PLACEHOLDER_TIMESTAMP.to_string(), 0)
    } else {
        (
            format_timestamp(start_wall, entry.elapsed_ms),
            entry.elapsed_ms,
        )
    };

    let message = ctx.map_or_else(
        || entry.message.clone(),
        |ctx| ctx.normalize_string(&entry.message),
    );
    let category = ctx.map_or_else(
        || entry.category.clone(),
        |ctx| ctx.normalize_string(&entry.category),
    );

    let mut context = BTreeMap::new();
    for (key, value) in &entry.context {
        let value = ctx.map_or_else(|| value.clone(), |ctx| ctx.normalize_string(value));
        context.insert(key.clone(), value);
    }

    TestLogJsonRecord {
        schema: TEST_LOG_SCHEMA,
        record_type: "log",
        test: test_name.map(ToString::to_string),
        seq,
        ts,
        t_ms,
        level: entry.level.as_json_str(),
        category,
        message,
        context,
    }
}

fn build_artifact_record(
    artifact: &ArtifactEntry,
    seq: usize,
    test_name: Option<&str>,
    ctx: Option<&NormalizationContext>,
    start_wall: SystemTime,
    normalized: bool,
) -> TestArtifactJsonRecord {
    let (ts, t_ms) = if normalized {
        (PLACEHOLDER_TIMESTAMP.to_string(), 0)
    } else {
        (
            format_timestamp(start_wall, artifact.elapsed_ms),
            artifact.elapsed_ms,
        )
    };
    let path = ctx.map_or_else(
        || artifact.path.clone(),
        |ctx| ctx.normalize_string(&artifact.path),
    );
    let name = ctx.map_or_else(
        || artifact.name.clone(),
        |ctx| ctx.normalize_string(&artifact.name),
    );
    let (size_bytes, sha256) = artifact_metadata(Path::new(&artifact.path));

    TestArtifactJsonRecord {
        schema: TEST_ARTIFACT_SCHEMA,
        record_type: "artifact",
        test: test_name.map(ToString::to_string),
        seq,
        ts,
        t_ms,
        name,
        path,
        size_bytes,
        sha256,
    }
}

fn format_timestamp(start_wall: SystemTime, elapsed_ms: u64) -> String {
    let ts = start_wall
        .checked_add(Duration::from_millis(elapsed_ms))
        .unwrap_or(start_wall);
    let ts: DateTime<Utc> = ts.into();
    ts.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn artifact_metadata(path: &Path) -> (Option<u64>, Option<String>) {
    let size_bytes = fs::metadata(path).map(|meta| meta.len()).ok();
    let sha256 = sha256_file(path).ok();
    (size_bytes, sha256)
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    let digest = hasher.finalize();
    Ok(to_hex(&digest))
}

fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn write_string_to_path(path: &Path, contents: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    fs::write(path, contents)
}

/// Macro for logging with automatic context capture.
///
/// # Example
///
/// ```ignore
/// log_ctx!(logger, Info, "action", "Processing file",
///     "path" => file_path.display(),
///     "size" => file_size
/// );
/// ```
#[macro_export]
macro_rules! log_ctx {
    ($logger:expr, $level:ident, $category:expr, $message:expr, $($key:expr => $value:expr),* $(,)?) => {
        $logger.with_context(
            $crate::common::logging::LogLevel::$level,
            $category,
            $message,
            |ctx| {
                $(
                    ctx.push(($key.to_string(), format!("{}", $value)));
                )*
            }
        );
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_logging() {
        let logger = TestLogger::new();

        logger.info("setup", "Test started");
        logger.debug("details", "Extra info");
        logger.warn("check", "Something suspicious");
        logger.error("fail", "Something broke");

        assert_eq!(logger.entry_count(), 4);
        assert!(logger.has_errors());

        let dump = logger.dump();
        assert!(dump.contains("Test started"));
        assert!(dump.contains("Something broke"));
    }

    #[test]
    fn test_context_logging() {
        let logger = TestLogger::new();

        logger.info_ctx("action", "Processing", |ctx| {
            ctx.push(("file".into(), "test.txt".into()));
            ctx.push(("size".into(), "1024".into()));
        });

        let dump = logger.dump();
        assert!(dump.contains("Processing"));
        assert!(dump.contains("file = test.txt"));
        assert!(dump.contains("size = 1024"));
    }

    #[test]
    fn test_min_level_filtering() {
        let logger = TestLogger::with_min_level(LogLevel::Warn);

        logger.debug("test", "Debug message");
        logger.info("test", "Info message");
        logger.warn("test", "Warn message");
        logger.error("test", "Error message");

        assert_eq!(logger.entry_count(), 2);

        let dump = logger.dump();
        assert!(!dump.contains("Debug message"));
        assert!(!dump.contains("Info message"));
        assert!(dump.contains("Warn message"));
        assert!(dump.contains("Error message"));
    }

    #[test]
    fn test_colored_output() {
        let logger = TestLogger::new();
        logger.info("test", "Colored message");

        let colored = logger.dump_colored();
        assert!(colored.contains("\x1b[")); // Contains ANSI codes
    }

    #[test]
    fn test_error_messages() {
        let logger = TestLogger::new();

        logger.error("fail", "First error");
        logger.info("ok", "Some info");
        logger.error("fail", "Second error");

        let errors = logger.error_messages();
        assert_eq!(errors.len(), 2);
        assert_eq!(errors[0], "First error");
        assert_eq!(errors[1], "Second error");
    }

    #[test]
    fn test_redaction() {
        let logger = TestLogger::new();
        logger.info_ctx("auth", "Headers", |ctx| {
            ctx.push(("Authorization".into(), "Bearer secret".into()));
            ctx.push(("path".into(), "/tmp/file.txt".into()));
        });

        let dump = logger.dump();
        assert!(dump.contains("Authorization = [REDACTED]"));
        assert!(dump.contains("path = /tmp/file.txt"));
    }

    #[test]
    fn test_artifact_logging() {
        let logger = TestLogger::new();
        logger.record_artifact("trace", "/tmp/trace.json");

        let artifacts = logger.dump_artifacts();
        assert!(artifacts.contains("trace"));
        assert!(artifacts.contains("/tmp/trace.json"));
    }

    #[test]
    fn jsonl_dump_includes_logs_and_artifacts_with_normalization() {
        let logger = TestLogger::new();
        logger.set_normalization_root("/tmp/my-root");

        logger.info_ctx("harness", "created", |ctx| {
            ctx.push(("path".into(), "/tmp/my-root/work.txt".into()));
        });
        logger.record_artifact("log", "/tmp/my-root/log.txt");

        let jsonl = logger.dump_jsonl_normalized();
        let mut lines = jsonl.lines();
        let first: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        let second: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();

        assert_eq!(first["schema"], TEST_LOG_SCHEMA);
        assert_eq!(second["schema"], TEST_ARTIFACT_SCHEMA);
        assert_eq!(first["type"], "log");
        assert_eq!(second["type"], "artifact");
        assert_eq!(first["seq"], 1);
        assert_eq!(second["seq"], 2);
        assert_eq!(first["ts"], PLACEHOLDER_TIMESTAMP);
        assert_eq!(second["ts"], PLACEHOLDER_TIMESTAMP);
        assert!(jsonl.contains(PLACEHOLDER_TEST_ROOT));
    }
}
