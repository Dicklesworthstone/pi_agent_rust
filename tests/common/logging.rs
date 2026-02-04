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

use regex::Regex;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime};

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
    /// Elapsed time from logger creation.
    pub elapsed_secs: f64,
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
        let mut output = format!(
            "[{:>8.3}s] {} [{}] {}\n",
            self.elapsed_secs,
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

        let mut output = format!(
            "{DIM}[{:>8.3}s]{RESET} {}{}{RESET} {DIM}[{}]{RESET} {}\n",
            self.elapsed_secs,
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
    /// Elapsed time from logger creation.
    pub elapsed_secs: f64,
    /// Logical name of the artifact.
    pub name: String,
    /// Path to the artifact on disk.
    pub path: String,
}

impl ArtifactEntry {
    /// Format this artifact entry as a string.
    pub fn format(&self) -> String {
        format!(
            "[{:>8.3}s] {} -> {}\n",
            self.elapsed_secs, self.name, self.path
        )
    }
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
    size_bytes: Option<u64>,
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
        let test_root = test_root
            .and_then(|root| root.canonicalize().ok())
            .map(|root| root.display().to_string());
        Self {
            project_root,
            test_root,
        }
    }
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

    /// Log an entry with the given level and category.
    pub fn log(&self, level: LogLevel, category: &str, message: impl Into<String>) {
        if (level as u8) < (self.min_level as u8) {
            return;
        }

        let entry = LogEntry {
            elapsed_secs: self.start.elapsed().as_secs_f64(),
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
            elapsed_secs: self.start.elapsed().as_secs_f64(),
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
            elapsed_secs: self.start.elapsed().as_secs_f64(),
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

    /// Dump logs and artifacts as stable JSONL (one JSON object per line).
    ///
    /// This output is intended for machine parsing and deterministic diffs. It:
    /// - uses elapsed milliseconds (no wall clock)
    /// - redacts sensitive context values (same rules as text)
    /// - optionally normalizes paths under a configured root (`set_normalization_root`)
    pub fn dump_jsonl(&self) -> String {
        #[derive(Debug, serde::Serialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        enum JsonlRecord<'a> {
            Log {
                t_ms: u64,
                level: &'a str,
                category: &'a str,
                message: &'a str,
                #[serde(skip_serializing_if = "Option::is_none")]
                context: Option<&'a [(String, String)]>,
            },
            Artifact {
                t_ms: u64,
                name: &'a str,
                path: &'a str,
            },
        }

        let normalize_root = self.normalize_root.lock().unwrap().clone();
        let entries = self.entries.lock().unwrap();
        let artifacts = self.artifacts.lock().unwrap();

        let mut out = String::with_capacity((entries.len() + artifacts.len()).saturating_mul(128));

        for entry in entries.iter() {
            let ctx = if entry.context.is_empty() {
                None
            } else {
                Some(entry.context.as_slice())
            };
            let record = JsonlRecord::Log {
                t_ms: (entry.elapsed_secs * 1000.0).round() as u64,
                level: entry.level.as_str().trim(),
                category: &entry.category,
                message: &entry.message,
                context: ctx,
            };
            let mut line =
                serde_json::to_string(&record).unwrap_or_else(|_| "{\"type\":\"log\"}".to_string());
            normalize_jsonl_line_in_place(&mut line, normalize_root.as_deref());
            out.push_str(&line);
            out.push('\n');
        }

        for artifact in artifacts.iter() {
            let record = JsonlRecord::Artifact {
                t_ms: (artifact.elapsed_secs * 1000.0).round() as u64,
                name: &artifact.name,
                path: &artifact.path,
            };
            let mut line = serde_json::to_string(&record)
                .unwrap_or_else(|_| "{\"type\":\"artifact\"}".to_string());
            normalize_jsonl_line_in_place(&mut line, normalize_root.as_deref());
            out.push_str(&line);
            out.push('\n');
        }

        drop(artifacts);
        drop(entries);

        out
    }

    /// Write JSONL dump to a file path.
    pub fn write_jsonl_to_path(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        fs::write(path, self.dump_jsonl())
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

fn normalize_jsonl_line_in_place(line: &mut String, normalize_root: Option<&str>) {
    if let Some(root) = normalize_root {
        if !root.is_empty() {
            *line = line.replace(root, "<root>");
        }
    }
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

        let jsonl = logger.dump_jsonl();
        assert!(jsonl.contains("\"type\":\"log\""));
        assert!(jsonl.contains("\"type\":\"artifact\""));
        assert!(jsonl.contains("<root>/work.txt"));
        assert!(jsonl.contains("<root>/log.txt"));
    }
}
