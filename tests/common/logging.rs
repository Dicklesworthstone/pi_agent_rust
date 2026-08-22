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
use std::path::{Component, Path};
use std::sync::atomic::{AtomicU64, Ordering};
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

/// Deprecated: new tests MUST use v2. This constant exists only for backward-compat
/// validation of existing test data. See DISC-021 / bd-38m8w.
const TEST_LOG_SCHEMA_V1: &str = "pi.test.log.v1";
/// Canonical log schema for all new test JSONL records.
pub const TEST_LOG_SCHEMA_V2: &str = "pi.test.log.v2";
/// Canonical artifact index schema for JSONL artifact records.
pub const TEST_ARTIFACT_SCHEMA_V1: &str = "pi.test.artifact.v1";
/// Canonical evidence contract schema for aggregate e2e run evidence.
pub const EVIDENCE_CONTRACT_SCHEMA_V1: &str = "pi.qa.evidence_contract.v1";
/// Canonical failure digest schema used for failing suites.
pub const FAILURE_DIGEST_SCHEMA_V1: &str = "pi.e2e.failure_digest.v1";
/// Versioned parity test/logging contract identifier.
pub const PARITY_TEST_LOGGING_CONTRACT_SCHEMA_V1: &str = "pi.parity.test_logging_contract.v1";
/// Unified structured logging contract tying all schemas together (bd-3ar8v.1.7).
pub const EVIDENCE_LOGGING_CONTRACT_SCHEMA_V1: &str = "pi.test.evidence_logging_contract.v1";
/// Performance evidence record schema for PERF-3X beads (bd-3ar8v.1.7).
pub const PERF_EVIDENCE_SCHEMA_V1: &str = "pi.perf.evidence.v1";
/// Bead-to-test coverage link schema for coverage auditing (bd-3ar8v.1.7).
pub const BEAD_COVERAGE_SCHEMA_V1: &str = "pi.perf.bead_coverage.v1";
const TEST_LOG_SCHEMA: &str = TEST_LOG_SCHEMA_V2;
const TEST_ARTIFACT_SCHEMA: &str = TEST_ARTIFACT_SCHEMA_V1;
const PLACEHOLDER_TIMESTAMP: &str = "<TIMESTAMP>";
const PLACEHOLDER_PROJECT_ROOT: &str = "<PROJECT_ROOT>";
const PLACEHOLDER_TEST_ROOT: &str = "<TEST_ROOT>";
const PLACEHOLDER_RUN_ID: &str = "<RUN_ID>";
const PLACEHOLDER_UUID: &str = "<UUID>";
const PLACEHOLDER_PORT: &str = "<PORT>";
const PLACEHOLDER_TRACE_ID: &str = "<TRACE_ID>";
const PLACEHOLDER_SPAN_ID: &str = "<SPAN_ID>";

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
    /// Span ID if this entry was logged within a span.
    pub span_id: Option<String>,
    /// Parent span ID for nested span hierarchy.
    pub parent_span_id: Option<String>,
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

/// Performance evidence record linking a benchmark result to the unified contract (bd-3ar8v.1.7).
///
/// Each PERF-3X bead's test must emit at least one `PerfEvidenceRecord` proving it
/// produced measurable results. Consumed by bd-3ar8v.6.11 (coverage audit) and
/// bd-3ar8v.1.3 (`NO_DATA` → hard failure).
#[derive(Debug, Clone, Serialize)]
pub struct PerfEvidenceRecord {
    /// Schema identifier (always `pi.perf.evidence.v1`).
    pub schema: &'static str,
    /// Bead ID this evidence is linked to.
    pub bead_id: String,
    /// Record type classification.
    pub record_type: PerfRecordType,
    /// Correlation ID for cross-schema linking.
    pub correlation_id: String,
    /// ISO-8601 timestamp when the evidence was captured.
    pub timestamp: String,
    /// Environment fingerprint for reproducibility.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env_fingerprint: Option<BTreeMap<String, String>>,
    /// Partition tag (matched-state or realistic).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub partition: Option<String>,
    /// Scenario identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scenario_id: Option<String>,
}

/// Classification of performance evidence record types.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PerfRecordType {
    /// Raw benchmark timing data.
    BenchRun,
    /// Budget threshold check result.
    BudgetCheck,
    /// Regression delta against baseline.
    RegressionDelta,
    /// Realistic workload throughput/latency.
    WorkloadResult,
    /// Cross-runtime comparison report.
    ComparisonReport,
}

impl PerfEvidenceRecord {
    /// Create a new perf evidence record for a bead.
    pub fn new(bead_id: impl Into<String>, record_type: PerfRecordType) -> Self {
        let now: DateTime<Utc> = SystemTime::now().into();
        Self {
            schema: PERF_EVIDENCE_SCHEMA_V1,
            bead_id: bead_id.into(),
            record_type,
            correlation_id: generate_trace_id(),
            timestamp: now.to_rfc3339_opts(SecondsFormat::Millis, true),
            env_fingerprint: None,
            partition: None,
            scenario_id: None,
        }
    }

    /// Serialize to a JSONL line.
    pub fn to_jsonl_line(&self) -> String {
        serde_json::to_string(self).expect("PerfEvidenceRecord serialization")
    }
}

/// Bead-to-test coverage link for the coverage audit (bd-3ar8v.1.7 / bd-3ar8v.6.11).
///
/// Maps a bead to the test files and log artifacts that prove its implementation.
#[derive(Debug, Clone, Serialize)]
pub struct BeadCoverageLink {
    /// Schema identifier (always `pi.perf.bead_coverage.v1`).
    pub schema: &'static str,
    /// Bead ID being linked.
    pub bead_id: String,
    /// Test source files providing evidence.
    pub test_files: Vec<String>,
    /// JSONL log artifact paths.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub log_artifacts: Vec<String>,
    /// Evidence type classification.
    pub evidence_type: EvidenceType,
}

/// Classification of evidence types for bead coverage.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceType {
    Unit,
    Vcr,
    E2e,
    Bench,
    Regression,
    DesignDoc,
}

impl BeadCoverageLink {
    /// Create a new bead coverage link with fail-closed path hygiene validation.
    pub fn try_new(
        bead_id: impl Into<String>,
        test_files: &[String],
        evidence_type: EvidenceType,
    ) -> Result<Self, String> {
        let bead_id = bead_id.into().trim().to_string();
        if bead_id.is_empty() {
            return Err("bead_id must not be empty".to_string());
        }

        let test_files = validate_required_coverage_path_set(test_files, "test_files")?;

        Ok(Self {
            schema: BEAD_COVERAGE_SCHEMA_V1,
            bead_id,
            test_files,
            log_artifacts: Vec::new(),
            evidence_type,
        })
    }

    /// Create a new bead coverage link.
    pub fn new(
        bead_id: impl Into<String>,
        test_files: &[String],
        evidence_type: EvidenceType,
    ) -> Self {
        Self::try_new(bead_id, test_files, evidence_type)
            .expect("BeadCoverageLink::new requires repo-relative, traversal-safe test file paths")
    }

    /// Validate that all evidence paths are repo-relative and traversal-safe.
    pub fn validate_path_hygiene(&self) -> Result<(), String> {
        let _ = validate_required_coverage_path_set(&self.test_files, "test_files")?;
        let _ = validate_coverage_path_set(&self.log_artifacts, "log_artifacts")?;
        Ok(())
    }

    /// Serialize to a JSONL line.
    pub fn to_jsonl_line(&self) -> String {
        self.validate_path_hygiene().expect(
            "BeadCoverageLink::to_jsonl_line requires repo-relative, traversal-safe evidence paths",
        );
        serde_json::to_string(self).expect("BeadCoverageLink serialization")
    }
}

fn validate_coverage_path_set(paths: &[String], field_name: &str) -> Result<Vec<String>, String> {
    let mut normalized = Vec::with_capacity(paths.len());
    let mut seen = BTreeMap::new();
    for (index, path) in paths.iter().enumerate() {
        let normalized_path = validate_coverage_path(path, field_name, index)?;
        if let Some(first_index) = seen.insert(normalized_path.clone(), index) {
            return Err(format!(
                "{field_name}[{index}] duplicates {field_name}[{first_index}]: {normalized_path}"
            ));
        }
        normalized.push(normalized_path);
    }
    Ok(normalized)
}

fn validate_required_coverage_path_set(
    paths: &[String],
    field_name: &str,
) -> Result<Vec<String>, String> {
    if paths.is_empty() {
        return Err(format!("{field_name} must contain at least one path"));
    }
    validate_coverage_path_set(paths, field_name)
}

fn validate_coverage_path(path: &str, field_name: &str, index: usize) -> Result<String, String> {
    let candidate = path.trim();
    if candidate.is_empty() {
        return Err(format!("{field_name}[{index}] must not be empty"));
    }

    let normalized = normalize_coverage_path(candidate);
    if normalized.is_empty() || normalized == "." {
        return Err(format!(
            "{field_name}[{index}] must resolve to a repo-relative file path, got: {candidate}"
        ));
    }
    let parsed = Path::new(&normalized);
    // Repo-relative means the same string must resolve inside the repo on
    // every platform, so the rejection checks operate on the normalized
    // string rather than the host platform's `Path` semantics (gh #175: on
    // Windows `Path::is_absolute()` requires a drive/UNC prefix, so a lone
    // leading `/` — rooted but prefix-less — sailed through). After
    // `normalize_coverage_path` every `\` is `/` and every `//` run is
    // collapsed, so a single `starts_with('/')` also covers `\foo`,
    // `//server/share`, and `\\server\share` forms. Any `X:` drive prefix
    // (absolute `C:/x` or drive-relative `C:x`) is likewise non-portable.
    if parsed.is_absolute()
        || normalized.starts_with('/')
        || looks_like_windows_drive_path(&normalized)
    {
        return Err(format!(
            "{field_name}[{index}] must be repo-relative, got: {candidate}"
        ));
    }

    if parsed
        .components()
        .any(|component| component == Component::ParentDir)
    {
        return Err(format!(
            "{field_name}[{index}] must not contain '..' traversal: {candidate}"
        ));
    }

    Ok(normalized)
}

fn normalize_coverage_path(candidate: &str) -> String {
    let mut normalized = candidate.replace('\\', "/");
    while let Some(stripped) = normalized.strip_prefix("./") {
        normalized = stripped.to_string();
    }
    while normalized.contains("//") {
        normalized = normalized.replace("//", "/");
    }
    normalized
}

/// Detect any Windows drive prefix: absolute (`C:/x`, `C:\x`) and
/// drive-relative (`C:x`) forms alike. A drive prefix can never appear in a
/// portable repo-relative path, so all forms fail closed (gh #175).
const fn looks_like_windows_drive_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
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
    trace_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    span_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_span_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    test: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ci_correlation_id: Option<String>,
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

/// Active span on the logger's span stack.
#[derive(Debug, Clone)]
struct SpanInfo {
    id: String,
    parent_id: Option<String>,
    name: String,
    started_ms: u64,
}

/// RAII guard that ends a span when dropped.
///
/// Created by [`TestLogger::begin_span`]. When the guard is dropped,
/// an automatic "span.end" log entry is emitted with duration info.
pub struct SpanGuard<'a> {
    logger: &'a TestLogger,
    span_id: String,
    name: String,
}

impl Drop for SpanGuard<'_> {
    fn drop(&mut self) {
        self.logger.end_span_internal(&self.span_id, &self.name);
    }
}

impl SpanGuard<'_> {
    /// Returns the span ID of this guard.
    #[must_use]
    pub fn span_id(&self) -> &str {
        &self.span_id
    }
}

/// Thread-safe test logger that captures all log entries.
///
/// Entries are stored in memory and can be dumped on test failure.
/// The logger is designed to have minimal overhead during normal test execution.
///
/// # Correlation Model
///
/// Every logger instance has a unique `trace_id` that tags all emitted JSONL
/// records, enabling correlation of logs from the same test run. Logs emitted
/// within a span carry a `span_id` (and optional `parent_span_id` for nesting).
///
/// ```ignore
/// let logger = TestLogger::new();
/// let span = logger.begin_span("load_extension");
/// logger.info("ext", "Loading my-ext");   // tagged with span-1
/// drop(span);                              // auto-logs span.end
/// ```
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
    /// Unique trace ID for this logger instance (correlates all records).
    trace_id: String,
    /// Optional CI correlation ID from `CI_CORRELATION_ID` env var.
    ci_correlation_id: Option<String>,
    /// Stack of active spans (most recent last).
    active_spans: Mutex<Vec<SpanInfo>>,
    /// Monotonic counter for generating span IDs.
    next_span_seq: AtomicU64,
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
    /// A unique `trace_id` is generated automatically.
    #[must_use]
    pub fn new() -> Self {
        let ci_correlation_id = std::env::var("CI_CORRELATION_ID")
            .ok()
            .filter(|s| !s.is_empty());
        Self {
            entries: Mutex::new(Vec::with_capacity(256)),
            artifacts: Mutex::new(Vec::with_capacity(16)),
            start: Instant::now(),
            start_wall: SystemTime::now(),
            min_level: LogLevel::Debug,
            test_name: Mutex::new(None),
            normalize_root: Mutex::new(None),
            trace_id: generate_trace_id(),
            ci_correlation_id,
            active_spans: Mutex::new(Vec::new()),
            next_span_seq: AtomicU64::new(1),
        }
    }

    /// Create a logger that only captures entries at or above the given level.
    #[must_use]
    pub fn with_min_level(min_level: LogLevel) -> Self {
        let ci_correlation_id = std::env::var("CI_CORRELATION_ID")
            .ok()
            .filter(|s| !s.is_empty());
        Self {
            entries: Mutex::new(Vec::with_capacity(256)),
            artifacts: Mutex::new(Vec::with_capacity(16)),
            start: Instant::now(),
            start_wall: SystemTime::now(),
            min_level,
            test_name: Mutex::new(None),
            normalize_root: Mutex::new(None),
            trace_id: generate_trace_id(),
            ci_correlation_id,
            active_spans: Mutex::new(Vec::new()),
            next_span_seq: AtomicU64::new(1),
        }
    }

    /// Returns the trace ID for this logger instance.
    #[must_use]
    pub fn trace_id(&self) -> &str {
        &self.trace_id
    }

    /// Returns the CI correlation ID, if set via `CI_CORRELATION_ID` env var.
    #[must_use]
    pub fn ci_correlation_id(&self) -> Option<&str> {
        self.ci_correlation_id.as_deref()
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

    /// Begin a named span. Returns a [`SpanGuard`] that automatically
    /// ends the span when dropped, emitting a "span.end" log entry.
    ///
    /// Logs emitted while the span is active carry the span's ID.
    /// Spans can be nested; child spans record their parent's ID.
    pub fn begin_span(&self, name: &str) -> SpanGuard<'_> {
        let seq = self.next_span_seq.fetch_add(1, Ordering::Relaxed);
        let span_id = format!("span-{seq}");
        let parent_id = self
            .active_spans
            .lock()
            .unwrap()
            .last()
            .map(|s| s.id.clone());

        let info = SpanInfo {
            id: span_id.clone(),
            parent_id: parent_id.clone(),
            name: name.to_string(),
            started_ms: self.elapsed_ms(),
        };
        self.active_spans.lock().unwrap().push(info);

        // Log span.begin
        let entry = LogEntry {
            elapsed_ms: self.elapsed_ms(),
            level: LogLevel::Debug,
            category: "span.begin".to_string(),
            message: name.to_string(),
            context: Vec::new(),
            span_id: Some(span_id.clone()),
            parent_span_id: parent_id,
        };
        self.entries.lock().unwrap().push(entry);

        SpanGuard {
            logger: self,
            span_id,
            name: name.to_string(),
        }
    }

    /// Internal: end a span and emit span.end log entry.
    fn end_span_internal(&self, span_id: &str, name: &str) {
        let started_ms = {
            let mut spans = self.active_spans.lock().unwrap();
            let started = spans
                .iter()
                .rfind(|s| s.id == span_id)
                .map(|s| s.started_ms);
            spans.retain(|s| s.id != span_id);
            started
        };
        let elapsed = self.elapsed_ms();
        let duration_ms = started_ms.map_or(0, |s| elapsed.saturating_sub(s));
        let parent_id = self
            .active_spans
            .lock()
            .unwrap()
            .last()
            .map(|s| s.id.clone());

        let entry = LogEntry {
            elapsed_ms: elapsed,
            level: LogLevel::Debug,
            category: "span.end".to_string(),
            message: name.to_string(),
            context: vec![("duration_ms".to_string(), duration_ms.to_string())],
            span_id: Some(span_id.to_string()),
            parent_span_id: parent_id,
        };
        self.entries.lock().unwrap().push(entry);
    }

    /// Returns the current active span ID, if any.
    fn current_span(&self) -> (Option<String>, Option<String>) {
        let spans = self.active_spans.lock().unwrap();
        spans.last().map_or((None, None), |span| {
            (Some(span.id.clone()), span.parent_id.clone())
        })
    }

    /// Log an entry with the given level and category.
    pub fn log(&self, level: LogLevel, category: &str, message: impl Into<String>) {
        if (level as u8) < (self.min_level as u8) {
            return;
        }

        let (span_id, parent_span_id) = self.current_span();
        let entry = LogEntry {
            elapsed_ms: self.elapsed_ms(),
            level,
            category: category.to_string(),
            message: message.into(),
            context: Vec::new(),
            span_id,
            parent_span_id,
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

        let (span_id, parent_span_id) = self.current_span();
        let entry = LogEntry {
            elapsed_ms: self.elapsed_ms(),
            level,
            category: category.to_string(),
            message: message.into(),
            context,
            span_id,
            parent_span_id,
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
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
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
    /// - includes a schema tag (`pi.test.log.v2` / `pi.test.artifact.v1`)
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
                    &self.trace_id,
                    self.ci_correlation_id.as_deref(),
                    ctx.as_ref(),
                    self.start_wall,
                    normalized,
                );
                seq = seq.saturating_add(1);
                let line = serde_json::to_string(&record)
                    .unwrap_or_else(|_| "{\"schema\":\"pi.test.log.v2\"}".to_string());
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

/// Generate a unique trace ID for a logger instance.
///
/// Uses a combination of system time + monotonic counter to ensure uniqueness
/// even when multiple loggers are created in the same millisecond.
fn generate_trace_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let ts = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut hasher = Sha256::new();
    hasher.update(ts.to_le_bytes());
    hasher.update(seq.to_le_bytes());
    let digest = hasher.finalize();
    // Use first 16 bytes (128 bits) formatted as hex for a compact trace ID.
    to_hex(&digest[..16])
}

fn redact_context(context: &mut [(String, String)]) {
    for (key, value) in context.iter_mut() {
        if is_sensitive_key(key) {
            *value = REDACTED_VALUE.to_string();
        }
    }
}

/// Keys that contain a redaction-key substring but are known-safe API
/// parameters (e.g. `max_tokens`, `inputTokens`, `totalTokenCount`).
/// Keys that contain a redaction-key substring but are known-safe API
/// parameters (token counts, budget configs, etc.).
const SAFE_TOKEN_KEYS: [&str; 9] = [
    "max_tokens",
    "budget_tokens",
    "inputtokens",
    "outputtokens",
    "totaltokens",
    "candidatestokencount",
    "prompttokencount",
    "totaltokencount",
    "cachedcontenttokencount",
];

fn is_sensitive_key(key: &str) -> bool {
    let key = key.trim().to_ascii_lowercase();
    if SAFE_TOKEN_KEYS.iter().any(|safe| key == *safe) {
        return false;
    }
    REDACTION_KEYS.iter().any(|needle| key.contains(needle))
}

#[allow(clippy::too_many_arguments)]
fn build_log_record(
    entry: &LogEntry,
    seq: usize,
    test_name: Option<&str>,
    trace_id: &str,
    ci_correlation_id: Option<&str>,
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

    let resolved_trace_id = if normalized {
        PLACEHOLDER_TRACE_ID.to_string()
    } else {
        trace_id.to_string()
    };
    let span_id = if normalized {
        entry
            .span_id
            .as_ref()
            .map(|_| PLACEHOLDER_SPAN_ID.to_string())
    } else {
        entry.span_id.clone()
    };
    let parent_span_id = if normalized {
        entry
            .parent_span_id
            .as_ref()
            .map(|_| PLACEHOLDER_SPAN_ID.to_string())
    } else {
        entry.parent_span_id.clone()
    };

    TestLogJsonRecord {
        schema: TEST_LOG_SCHEMA,
        record_type: "log",
        trace_id: resolved_trace_id,
        span_id,
        parent_span_id,
        test: test_name.map(ToString::to_string),
        ci_correlation_id: ci_correlation_id.map(ToString::to_string),
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
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, contents)
}

// ============================================================================
// JSONL Schema Validation
// ============================================================================

/// Required fields for a `pi.test.log.v1` JSONL record.
const LOG_RECORD_V1_REQUIRED_FIELDS: [&str; 8] = [
    "schema", "type", "seq", "ts", "t_ms", "level", "category", "message",
];

/// Required fields for a `pi.test.log.v2` JSONL record.
/// V2 adds `trace_id` as a required field; `span_id` and `parent_span_id` are optional.
const LOG_RECORD_V2_REQUIRED_FIELDS: [&str; 9] = [
    "schema", "type", "trace_id", "seq", "ts", "t_ms", "level", "category", "message",
];

/// Required fields for a `pi.test.artifact.v1` JSONL record.
const ARTIFACT_RECORD_REQUIRED_FIELDS: [&str; 7] =
    ["schema", "type", "seq", "ts", "t_ms", "name", "path"];

/// Validation error for a single JSONL record.
#[derive(Debug, Clone)]
pub struct JsonlValidationError {
    /// 1-based line number in the JSONL output.
    pub line: usize,
    /// The field that failed validation (or `<parse>` / `<root>`).
    pub field: String,
    /// Human-readable description of the problem.
    pub message: String,
}

impl std::fmt::Display for JsonlValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "line {}: field '{}': {}",
            self.line, self.field, self.message
        )
    }
}

/// Validate a single JSONL line against the appropriate schema.
///
/// Returns `Ok(())` if the record has all required fields with correct types,
/// or an error describing the first problem found.
pub fn validate_jsonl_line(line: &str, line_number: usize) -> Result<(), JsonlValidationError> {
    let value: serde_json::Value =
        serde_json::from_str(line).map_err(|err| JsonlValidationError {
            line: line_number,
            field: "<parse>".to_string(),
            message: format!("invalid JSON: {err}"),
        })?;

    let obj = value.as_object().ok_or_else(|| JsonlValidationError {
        line: line_number,
        field: "<root>".to_string(),
        message: "expected JSON object".to_string(),
    })?;

    let schema = obj.get("schema").and_then(|v| v.as_str()).unwrap_or("");
    let required: &[&str] = match schema {
        "pi.test.log.v1" => &LOG_RECORD_V1_REQUIRED_FIELDS,
        "pi.test.log.v2" => &LOG_RECORD_V2_REQUIRED_FIELDS,
        "pi.test.artifact.v1" => &ARTIFACT_RECORD_REQUIRED_FIELDS,
        _ => {
            return Err(JsonlValidationError {
                line: line_number,
                field: "schema".to_string(),
                message: format!("unknown schema: {schema:?}"),
            });
        }
    };

    for &field in required {
        if !obj.contains_key(field) {
            return Err(JsonlValidationError {
                line: line_number,
                field: field.to_string(),
                message: "required field missing".to_string(),
            });
        }
    }

    // Type checks for numeric/string fields.
    if let Some(seq) = obj.get("seq")
        && !seq.is_number()
    {
        return Err(JsonlValidationError {
            line: line_number,
            field: "seq".to_string(),
            message: format!("expected number, got {seq}"),
        });
    }
    if let Some(ts) = obj.get("ts")
        && !ts.is_string()
    {
        return Err(JsonlValidationError {
            line: line_number,
            field: "ts".to_string(),
            message: format!("expected string, got {ts}"),
        });
    }
    if let Some(t_ms) = obj.get("t_ms")
        && !t_ms.is_number()
    {
        return Err(JsonlValidationError {
            line: line_number,
            field: "t_ms".to_string(),
            message: format!("expected number, got {t_ms}"),
        });
    }

    // V2 type checks for correlation fields.
    if schema == "pi.test.log.v2" {
        if let Some(trace_id) = obj.get("trace_id")
            && !trace_id.is_string()
        {
            return Err(JsonlValidationError {
                line: line_number,
                field: "trace_id".to_string(),
                message: format!("expected string, got {trace_id}"),
            });
        }
        if let Some(span_id) = obj.get("span_id")
            && !span_id.is_string()
        {
            return Err(JsonlValidationError {
                line: line_number,
                field: "span_id".to_string(),
                message: format!("expected string, got {span_id}"),
            });
        }
        if let Some(parent_span_id) = obj.get("parent_span_id")
            && !parent_span_id.is_string()
        {
            return Err(JsonlValidationError {
                line: line_number,
                field: "parent_span_id".to_string(),
                message: format!("expected string, got {parent_span_id}"),
            });
        }
        if let Some(ci_correlation_id) = obj.get("ci_correlation_id")
            && !ci_correlation_id.is_string()
        {
            return Err(JsonlValidationError {
                line: line_number,
                field: "ci_correlation_id".to_string(),
                message: format!("expected string, got {ci_correlation_id}"),
            });
        }
    }

    Ok(())
}

/// Validate a single JSONL line, rejecting deprecated `pi.test.log.v1` records.
///
/// New test code MUST use v2. This function enforces that: v1 log records produce
/// an error, while v2 log and v1 artifact records pass normally. Use the standard
/// [`validate_jsonl_line`] for backward-compatible validation of grandfathered data.
///
/// See DISC-021 / bd-38m8w.
pub fn validate_jsonl_line_v2_only(
    line: &str,
    line_number: usize,
) -> Result<(), JsonlValidationError> {
    // First, run standard validation.
    validate_jsonl_line(line, line_number)?;

    // Then reject v1 log schema.
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(line)
        && let Some(schema) = value.get("schema").and_then(|v| v.as_str())
        && schema == TEST_LOG_SCHEMA_V1
    {
        return Err(JsonlValidationError {
            line: line_number,
            field: "schema".to_string(),
            message: format!("deprecated schema '{schema}': new tests must use {TEST_LOG_SCHEMA}"),
        });
    }

    Ok(())
}

/// Validate every non-empty line in a JSONL string, rejecting `pi.test.log.v1`.
///
/// Like [`validate_jsonl`] but enforces v2-only for log records. Use this for
/// new test code. See DISC-021 / bd-38m8w.
pub fn validate_jsonl_v2_only(content: &str) -> Vec<JsonlValidationError> {
    let mut errors = Vec::new();
    for (i, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Err(err) = validate_jsonl_line_v2_only(trimmed, i + 1) {
            errors.push(err);
        }
    }
    errors
}

/// Validate every non-empty line in a JSONL string.
///
/// Returns a (possibly empty) list of validation errors.
pub fn validate_jsonl(content: &str) -> Vec<JsonlValidationError> {
    let mut errors = Vec::new();
    for (i, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Err(err) = validate_jsonl_line(trimmed, i + 1) {
            errors.push(err);
        }
    }
    errors
}

/// Filter `log` JSONL records by category and standard context keys.
///
/// This supports cross-run triage flows that need to isolate a
/// scenario/component slice before comparing outputs.
pub fn filter_log_records(
    content: &str,
    category: Option<&str>,
    scenario_id: Option<&str>,
    component: Option<&str>,
) -> Vec<serde_json::Value> {
    let mut filtered = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        let Some(obj) = value.as_object() else {
            continue;
        };
        if obj.get("type").and_then(|v| v.as_str()) != Some("log") {
            continue;
        }

        let category_matches = category
            .is_none_or(|needle| obj.get("category").and_then(|v| v.as_str()) == Some(needle));
        let ctx = obj.get("context").and_then(|v| v.as_object());
        let scenario_matches = scenario_id.is_none_or(|needle| {
            ctx.and_then(|c| c.get("scenario_id"))
                .and_then(|v| v.as_str())
                == Some(needle)
        });
        let component_matches = component.is_none_or(|needle| {
            ctx.and_then(|c| c.get("component"))
                .and_then(|v| v.as_str())
                == Some(needle)
        });

        if category_matches && scenario_matches && component_matches {
            filtered.push(value);
        }
    }
    filtered
}

fn comparable_log_record(record: &serde_json::Value) -> Option<serde_json::Value> {
    let obj = record.as_object()?;
    let mut stable = serde_json::Map::new();
    for key in ["schema", "type", "level", "category", "message", "context"] {
        if let Some(value) = obj.get(key) {
            stable.insert(key.to_string(), value.clone());
        }
    }
    Some(serde_json::Value::Object(stable))
}

/// Compare two JSONL streams after filtering and stable-field projection.
///
/// Volatile correlation/timing fields are ignored by comparing only:
/// `schema`, `type`, `level`, `category`, `message`, and `context`.
pub fn compare_log_streams_by_filter(
    left: &str,
    right: &str,
    category: Option<&str>,
    scenario_id: Option<&str>,
    component: Option<&str>,
) -> bool {
    let left_records: Vec<_> = filter_log_records(left, category, scenario_id, component)
        .iter()
        .filter_map(comparable_log_record)
        .collect();
    let right_records: Vec<_> = filter_log_records(right, category, scenario_id, component)
        .iter()
        .filter_map(comparable_log_record)
        .collect();
    left_records == right_records
}

// ============================================================================
// Artifact-Index Path Cross-Validation (DISC-019 / GAP-3)
// ============================================================================

/// Warning emitted when an artifact-index record references a path that does
/// not exist on disk.
#[derive(Debug, Clone)]
pub struct ArtifactPathWarning {
    /// 1-based line number in the artifact-index JSONL.
    pub line: usize,
    /// The logical artifact name from the record.
    pub name: String,
    /// The path value that did not resolve.
    pub path: String,
}

impl std::fmt::Display for ArtifactPathWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "line {}: artifact '{}' path not found: {}",
            self.line, self.name, self.path
        )
    }
}

/// Cross-validate artifact-index JSONL paths against the filesystem.
///
/// Parses each `pi.test.artifact.v1` record and checks that the `path` field
/// resolves to an existing file, either as an absolute path or relative to
/// `artifact_dir`. Returns a list of warnings for paths that don't resolve.
///
/// Non-artifact records and unparseable lines are silently skipped (those are
/// validated separately by [`validate_jsonl`]).
pub fn validate_artifact_index_paths(
    artifact_index_jsonl: &str,
    artifact_dir: &Path,
) -> Vec<ArtifactPathWarning> {
    let mut warnings = Vec::new();
    for (i, line) in artifact_index_jsonl.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        let Some(obj) = value.as_object() else {
            continue;
        };
        let schema = obj.get("schema").and_then(|v| v.as_str()).unwrap_or("");
        if schema != TEST_ARTIFACT_SCHEMA {
            continue;
        }
        let Some(path_str) = obj.get("path").and_then(|v| v.as_str()) else {
            continue;
        };
        let name = obj
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("<unknown>");

        let path = Path::new(path_str);
        let exists = if path.is_absolute() {
            path.exists()
        } else {
            artifact_dir.join(path).exists()
        };

        if !exists {
            warnings.push(ArtifactPathWarning {
                line: i + 1,
                name: name.to_string(),
                path: path_str.to_string(),
            });
        }
    }
    warnings
}

// ============================================================================
// Deep JSON Redaction
// ============================================================================

/// Recursively redact sensitive keys inside a JSON value at any depth.
///
/// This is useful for sanitizing request/response bodies that may contain
/// API keys, bearer tokens, or credentials nested inside JSON payloads.
pub fn redact_json_value(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, val) in map.iter_mut() {
                if is_sensitive_key(key) {
                    *val = serde_json::Value::String(REDACTED_VALUE.to_string());
                } else {
                    redact_json_value(val);
                }
            }
        }
        serde_json::Value::Array(arr) => {
            for val in arr.iter_mut() {
                redact_json_value(val);
            }
        }
        _ => {}
    }
}

/// Scan a JSON value and return paths to any sensitive keys whose values
/// are not the redaction placeholder.
///
/// Useful as a test assertion: `assert!(find_unredacted_keys(&val).is_empty())`.
pub fn find_unredacted_keys(value: &serde_json::Value) -> Vec<String> {
    let mut unredacted = Vec::new();
    find_unredacted_keys_inner(value, "", &mut unredacted);
    unredacted
}

fn find_unredacted_keys_inner(value: &serde_json::Value, path: &str, unredacted: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, val) in map {
                let field_path = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                if is_sensitive_key(key) {
                    if val.as_str() != Some(REDACTED_VALUE) {
                        unredacted.push(field_path);
                    }
                } else {
                    find_unredacted_keys_inner(val, &field_path, unredacted);
                }
            }
        }
        serde_json::Value::Array(arr) => {
            for (i, val) in arr.iter().enumerate() {
                let field_path = format!("{path}[{i}]");
                find_unredacted_keys_inner(val, &field_path, unredacted);
            }
        }
        _ => {}
    }
}

// ============================================================================
// Cost-Budget Telemetry
// ============================================================================

/// Per-provider cost threshold (in US dollars).
#[derive(Debug, Clone)]
pub struct CostThreshold {
    /// Provider name (e.g., `"anthropic"`, `"openai"`).
    pub provider: String,
    /// Soft limit: log a warning when exceeded.
    pub warn_dollars: f64,
    /// Hard limit: fail the test when exceeded.
    pub fail_dollars: f64,
}

/// Outcome of a cost-budget check.
#[derive(Debug, Clone, PartialEq)]
pub enum CostBudgetOutcome {
    /// Cost is within budget.
    Ok,
    /// Cost exceeded the warning threshold but not the failure threshold.
    Warn {
        provider: String,
        cost: f64,
        threshold: f64,
    },
    /// Cost exceeded the hard failure threshold.
    Fail {
        provider: String,
        cost: f64,
        threshold: f64,
    },
}

impl std::fmt::Display for CostBudgetOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ok => write!(f, "cost within budget"),
            Self::Warn {
                provider,
                cost,
                threshold,
            } => write!(
                f,
                "WARNING: {provider} cost ${cost:.6} exceeds warn threshold ${threshold:.6}"
            ),
            Self::Fail {
                provider,
                cost,
                threshold,
            } => write!(
                f,
                "FAIL: {provider} cost ${cost:.6} exceeds fail threshold ${threshold:.6}"
            ),
        }
    }
}

/// Default per-call cost thresholds for live E2E tests.
///
/// These are conservative: a single "say hello" prompt with `max_tokens=64`
/// should cost well under $0.01.
#[must_use]
pub fn default_cost_thresholds() -> Vec<CostThreshold> {
    vec![
        CostThreshold {
            provider: "anthropic".to_string(),
            warn_dollars: 0.05,
            fail_dollars: 0.25,
        },
        CostThreshold {
            provider: "openai".to_string(),
            warn_dollars: 0.05,
            fail_dollars: 0.25,
        },
        CostThreshold {
            provider: "google".to_string(),
            warn_dollars: 0.05,
            fail_dollars: 0.25,
        },
        CostThreshold {
            provider: "openrouter".to_string(),
            warn_dollars: 0.10,
            fail_dollars: 0.50,
        },
        CostThreshold {
            provider: "xai".to_string(),
            warn_dollars: 0.05,
            fail_dollars: 0.25,
        },
        CostThreshold {
            provider: "deepseek".to_string(),
            warn_dollars: 0.05,
            fail_dollars: 0.25,
        },
    ]
}

/// Check a provider run's total cost against budget thresholds.
///
/// Returns [`CostBudgetOutcome::Ok`] if `total_cost` is under both limits,
/// [`CostBudgetOutcome::Warn`] if it exceeds the soft limit, or
/// [`CostBudgetOutcome::Fail`] if it exceeds the hard limit.
///
/// If no threshold is configured for the given provider, returns `Ok`.
#[must_use]
pub fn check_cost_budget(
    provider: &str,
    total_cost: f64,
    thresholds: &[CostThreshold],
) -> CostBudgetOutcome {
    let Some(threshold) = thresholds.iter().find(|t| t.provider == provider) else {
        return CostBudgetOutcome::Ok;
    };
    if total_cost >= threshold.fail_dollars {
        CostBudgetOutcome::Fail {
            provider: provider.to_string(),
            cost: total_cost,
            threshold: threshold.fail_dollars,
        }
    } else if total_cost >= threshold.warn_dollars {
        CostBudgetOutcome::Warn {
            provider: provider.to_string(),
            cost: total_cost,
            threshold: threshold.warn_dollars,
        }
    } else {
        CostBudgetOutcome::Ok
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

    // ====================================================================
    // JSONL Schema Validation
    // ====================================================================

    #[test]
    fn validate_jsonl_valid_log_record() {
        let record = r#"{"schema":"pi.test.log.v1","type":"log","seq":1,"ts":"2026-01-01T00:00:00.000Z","t_ms":0,"level":"info","category":"setup","message":"hello"}"#;
        assert!(validate_jsonl_line(record, 1).is_ok());
    }

    #[test]
    fn validate_jsonl_valid_artifact_record() {
        let record = r#"{"schema":"pi.test.artifact.v1","type":"artifact","seq":2,"ts":"2026-01-01T00:00:00.000Z","t_ms":0,"name":"trace","path":"/tmp/trace.json"}"#;
        assert!(validate_jsonl_line(record, 1).is_ok());
    }

    #[test]
    fn validate_jsonl_rejects_unknown_schema() {
        let record = r#"{"schema":"pi.test.unknown.v2","type":"log","seq":1,"ts":"x","t_ms":0}"#;
        let err = validate_jsonl_line(record, 1).unwrap_err();
        assert_eq!(err.field, "schema");
        assert!(err.message.contains("unknown schema"));
    }

    #[test]
    fn validate_jsonl_rejects_missing_required_field() {
        // Missing "message" field for a log record.
        let record = r#"{"schema":"pi.test.log.v1","type":"log","seq":1,"ts":"x","t_ms":0,"level":"info","category":"setup"}"#;
        let err = validate_jsonl_line(record, 1).unwrap_err();
        assert_eq!(err.field, "message");
        assert!(err.message.contains("required field missing"));
    }

    #[test]
    fn validate_jsonl_rejects_wrong_type_for_seq() {
        let record = r#"{"schema":"pi.test.log.v1","type":"log","seq":"not-a-number","ts":"x","t_ms":0,"level":"info","category":"setup","message":"hi"}"#;
        let err = validate_jsonl_line(record, 1).unwrap_err();
        assert_eq!(err.field, "seq");
        assert!(err.message.contains("expected number"));
    }

    #[test]
    fn validate_jsonl_rejects_invalid_json() {
        let err = validate_jsonl_line("{broken", 1).unwrap_err();
        assert_eq!(err.field, "<parse>");
    }

    #[test]
    fn validate_jsonl_rejects_non_object() {
        let err = validate_jsonl_line("[1,2,3]", 1).unwrap_err();
        assert_eq!(err.field, "<root>");
    }

    #[test]
    fn validate_jsonl_full_output_from_logger() {
        let logger = TestLogger::new();
        logger.info("setup", "test started");
        logger.warn("check", "something suspicious");
        logger.record_artifact("trace", "/tmp/trace.json");

        let jsonl = logger.dump_jsonl();
        let errors = validate_jsonl(&jsonl);
        assert!(
            errors.is_empty(),
            "logger output failed validation: {errors:?}"
        );
    }

    #[test]
    fn validate_jsonl_normalized_output_from_logger() {
        let logger = TestLogger::new();
        logger.set_normalization_root("/tmp/norm-root");
        logger.info("setup", "testing in /tmp/norm-root/workspace");
        logger.record_artifact("log", "/tmp/norm-root/run.log");

        let jsonl = logger.dump_jsonl_normalized();
        let errors = validate_jsonl(&jsonl);
        assert!(
            errors.is_empty(),
            "normalized logger output failed validation: {errors:?}"
        );
    }

    #[test]
    fn validate_jsonl_batch_collects_all_errors() {
        let bad_content = "{broken}\n[1,2]\n{\"schema\":\"pi.test.log.v1\",\"type\":\"log\",\"seq\":1,\"ts\":\"x\",\"t_ms\":0,\"level\":\"info\",\"category\":\"c\",\"message\":\"m\"}\n";
        let errors = validate_jsonl(bad_content);
        // First two lines are bad, third is valid.
        assert_eq!(errors.len(), 2);
        assert_eq!(errors[0].line, 1);
        assert_eq!(errors[1].line, 2);
    }

    #[test]
    fn filter_log_records_by_scenario_and_component() {
        let logger = TestLogger::new();
        logger.info_ctx("action", "scenario-A write", |ctx| {
            ctx.push(("scenario_id".into(), "scenario-A".into()));
            ctx.push(("component".into(), "session".into()));
        });
        logger.info_ctx("action", "scenario-B write", |ctx| {
            ctx.push(("scenario_id".into(), "scenario-B".into()));
            ctx.push(("component".into(), "session".into()));
        });
        logger.info_ctx("verify", "scenario-A verify", |ctx| {
            ctx.push(("scenario_id".into(), "scenario-A".into()));
            ctx.push(("component".into(), "rpc".into()));
        });

        let jsonl = logger.dump_jsonl();
        let filtered =
            filter_log_records(&jsonl, Some("action"), Some("scenario-A"), Some("session"));
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0]["message"], "scenario-A write");
    }

    #[test]
    fn compare_log_streams_by_filter_ignores_trace_and_timing() {
        let left = TestLogger::new();
        left.info_ctx("action", "persist queued message", |ctx| {
            ctx.push(("scenario_id".into(), "persist-backlog".into()));
            ctx.push(("component".into(), "session".into()));
            ctx.push(("sli_id".into(), "sli.persist.backlog_depth".into()));
        });

        let right = TestLogger::new();
        right.info_ctx("action", "persist queued message", |ctx| {
            ctx.push(("scenario_id".into(), "persist-backlog".into()));
            ctx.push(("component".into(), "session".into()));
            ctx.push(("sli_id".into(), "sli.persist.backlog_depth".into()));
        });

        assert!(compare_log_streams_by_filter(
            &left.dump_jsonl(),
            &right.dump_jsonl(),
            Some("action"),
            Some("persist-backlog"),
            Some("session"),
        ));
    }

    #[test]
    fn compare_log_streams_by_filter_detects_semantic_difference() {
        let left = TestLogger::new();
        left.info_ctx("action", "persist succeeded", |ctx| {
            ctx.push(("scenario_id".into(), "persist-backlog".into()));
            ctx.push(("component".into(), "session".into()));
        });

        let right = TestLogger::new();
        right.info_ctx("action", "persist failed", |ctx| {
            ctx.push(("scenario_id".into(), "persist-backlog".into()));
            ctx.push(("component".into(), "session".into()));
        });

        assert!(!compare_log_streams_by_filter(
            &left.dump_jsonl(),
            &right.dump_jsonl(),
            Some("action"),
            Some("persist-backlog"),
            Some("session"),
        ));
    }

    // ====================================================================
    // Deep JSON Redaction
    // ====================================================================

    #[test]
    fn redact_json_value_flat_object() {
        let mut val: serde_json::Value = serde_json::json!({
            "api_key": "sk-abc123",
            "model": "gpt-4",
            "authorization": "Bearer tok"
        });
        redact_json_value(&mut val);

        assert_eq!(val["api_key"], REDACTED_VALUE);
        assert_eq!(val["authorization"], REDACTED_VALUE);
        assert_eq!(val["model"], "gpt-4");
    }

    #[test]
    fn redact_json_value_nested_object() {
        let mut val: serde_json::Value = serde_json::json!({
            "request": {
                "headers": {
                    "Authorization": "Bearer secret-value",
                    "Content-Type": "application/json"
                },
                "body": {
                    "config": {
                        "api_key": "sk-live-nested-key",
                        "temperature": 0.7
                    }
                }
            }
        });
        redact_json_value(&mut val);

        assert_eq!(val["request"]["headers"]["Authorization"], REDACTED_VALUE);
        assert_eq!(
            val["request"]["headers"]["Content-Type"],
            "application/json"
        );
        assert_eq!(val["request"]["body"]["config"]["api_key"], REDACTED_VALUE);
        assert_eq!(val["request"]["body"]["config"]["temperature"], 0.7);
    }

    #[test]
    fn redact_json_value_array_with_nested_secrets() {
        let mut val: serde_json::Value = serde_json::json!([
            {"provider": "openai", "api_key": "sk-111"},
            {"provider": "anthropic", "api_key": "sk-222"},
            {"provider": "google", "token": "tok-333"}
        ]);
        redact_json_value(&mut val);

        assert_eq!(val[0]["api_key"], REDACTED_VALUE);
        assert_eq!(val[1]["api_key"], REDACTED_VALUE);
        assert_eq!(val[2]["token"], REDACTED_VALUE);
        assert_eq!(val[0]["provider"], "openai");
    }

    #[test]
    fn redact_json_value_all_sensitive_key_patterns() {
        let mut val: serde_json::Value = serde_json::json!({
            "api_key": "v1",
            "api-key": "v2",
            "authorization": "v3",
            "bearer": "v4",
            "cookie": "v5",
            "credential": "v6",
            "password": "v7",
            "private_key": "v8",
            "secret": "v9",
            "token": "v10"
        });
        redact_json_value(&mut val);

        for key in &REDACTION_KEYS {
            assert_eq!(
                val[key].as_str().unwrap(),
                REDACTED_VALUE,
                "key '{key}' was not redacted"
            );
        }
    }

    #[test]
    fn find_unredacted_keys_detects_leaks() {
        let val: serde_json::Value = serde_json::json!({
            "request": {
                "headers": {
                    "Authorization": "Bearer sk-live-leaked",
                    "Host": "api.openai.com"
                },
                "body": {
                    "api_key": "sk-also-leaked"
                }
            }
        });
        let leaks = find_unredacted_keys(&val);
        assert_eq!(leaks.len(), 2);
        assert!(leaks.iter().any(|p| p.contains("Authorization")));
        assert!(leaks.iter().any(|p| p.contains("api_key")));
    }

    #[test]
    fn find_unredacted_keys_empty_when_redacted() {
        let val: serde_json::Value = serde_json::json!({
            "api_key": REDACTED_VALUE,
            "authorization": REDACTED_VALUE,
            "model": "claude-3.5-sonnet"
        });
        assert!(find_unredacted_keys(&val).is_empty());
    }

    #[test]
    fn find_unredacted_keys_in_arrays() {
        let val: serde_json::Value = serde_json::json!({
            "items": [
                {"name": "a", "secret": "exposed"},
                {"name": "b", "secret": REDACTED_VALUE}
            ]
        });
        let leaks = find_unredacted_keys(&val);
        assert_eq!(leaks.len(), 1);
        assert!(leaks[0].contains("[0]"));
    }

    // ====================================================================
    // Cost-Budget Telemetry
    // ====================================================================

    #[test]
    fn cost_budget_ok_when_under_threshold() {
        let thresholds = default_cost_thresholds();
        let outcome = check_cost_budget("anthropic", 0.001, &thresholds);
        assert_eq!(outcome, CostBudgetOutcome::Ok);
    }

    #[test]
    fn cost_budget_warn_when_above_soft_limit() {
        let thresholds = default_cost_thresholds();
        let outcome = check_cost_budget("anthropic", 0.06, &thresholds);
        assert!(
            matches!(outcome, CostBudgetOutcome::Warn { .. }),
            "expected Warn, got {outcome:?}"
        );
    }

    #[test]
    fn cost_budget_fail_when_above_hard_limit() {
        let thresholds = default_cost_thresholds();
        let outcome = check_cost_budget("anthropic", 0.30, &thresholds);
        assert!(
            matches!(outcome, CostBudgetOutcome::Fail { .. }),
            "expected Fail, got {outcome:?}"
        );
    }

    #[test]
    fn cost_budget_ok_for_unknown_provider() {
        let thresholds = default_cost_thresholds();
        let outcome = check_cost_budget("some-unknown-provider", 999.0, &thresholds);
        assert_eq!(outcome, CostBudgetOutcome::Ok);
    }

    #[test]
    fn cost_budget_openrouter_has_higher_threshold() {
        let thresholds = default_cost_thresholds();
        // $0.08 is under openrouter's warn ($0.10) but above anthropic's ($0.05).
        let openrouter = check_cost_budget("openrouter", 0.08, &thresholds);
        let anthropic = check_cost_budget("anthropic", 0.08, &thresholds);
        assert_eq!(openrouter, CostBudgetOutcome::Ok);
        assert!(matches!(anthropic, CostBudgetOutcome::Warn { .. }));
    }

    #[test]
    fn cost_budget_exactly_at_warn_triggers_warn() {
        let thresholds = default_cost_thresholds();
        let outcome = check_cost_budget("openai", 0.05, &thresholds);
        assert!(
            matches!(outcome, CostBudgetOutcome::Warn { .. }),
            "expected Warn at exact threshold, got {outcome:?}"
        );
    }

    #[test]
    fn cost_budget_exactly_at_fail_triggers_fail() {
        let thresholds = default_cost_thresholds();
        let outcome = check_cost_budget("openai", 0.25, &thresholds);
        assert!(
            matches!(outcome, CostBudgetOutcome::Fail { .. }),
            "expected Fail at exact threshold, got {outcome:?}"
        );
    }

    #[test]
    fn cost_budget_zero_cost_ok() {
        let thresholds = default_cost_thresholds();
        let outcome = check_cost_budget("anthropic", 0.0, &thresholds);
        assert_eq!(outcome, CostBudgetOutcome::Ok);
    }

    #[test]
    fn cost_budget_display_format() {
        assert_eq!(CostBudgetOutcome::Ok.to_string(), "cost within budget");

        let warn = CostBudgetOutcome::Warn {
            provider: "openai".to_string(),
            cost: 0.06,
            threshold: 0.05,
        };
        let warn_str = warn.to_string();
        assert!(warn_str.contains("WARNING"));
        assert!(warn_str.contains("openai"));

        let fail = CostBudgetOutcome::Fail {
            provider: "anthropic".to_string(),
            cost: 0.30,
            threshold: 0.25,
        };
        let fail_str = fail.to_string();
        assert!(fail_str.contains("FAIL"));
        assert!(fail_str.contains("anthropic"));
    }

    #[test]
    fn cost_budget_custom_thresholds() {
        let custom = vec![CostThreshold {
            provider: "custom".to_string(),
            warn_dollars: 0.001,
            fail_dollars: 0.002,
        }];
        assert_eq!(
            check_cost_budget("custom", 0.0005, &custom),
            CostBudgetOutcome::Ok
        );
        assert!(matches!(
            check_cost_budget("custom", 0.0015, &custom),
            CostBudgetOutcome::Warn { .. }
        ));
        assert!(matches!(
            check_cost_budget("custom", 0.003, &custom),
            CostBudgetOutcome::Fail { .. }
        ));
    }

    #[test]
    fn default_cost_thresholds_covers_all_live_providers() {
        let thresholds = default_cost_thresholds();
        let expected = [
            "anthropic",
            "openai",
            "google",
            "openrouter",
            "xai",
            "deepseek",
        ];
        for provider in &expected {
            assert!(
                thresholds.iter().any(|t| t.provider == *provider),
                "missing threshold for provider '{provider}'"
            );
        }
    }

    // ====================================================================
    // Redaction + Context (existing context-level) additional coverage
    // ====================================================================

    #[test]
    fn redaction_case_insensitive_key_matching() {
        let logger = TestLogger::new();
        logger.info_ctx("auth", "Case test", |ctx| {
            ctx.push(("API_KEY".into(), "sk-123".into()));
            ctx.push(("Api-Key".into(), "sk-456".into()));
            ctx.push(("AUTHORIZATION".into(), "Bearer tok".into()));
            ctx.push(("Token".into(), "abc".into()));
        });
        let dump = logger.dump();
        assert!(dump.contains("API_KEY = [REDACTED]"));
        assert!(dump.contains("Api-Key = [REDACTED]"));
        assert!(dump.contains("AUTHORIZATION = [REDACTED]"));
        assert!(dump.contains("Token = [REDACTED]"));
    }

    #[test]
    fn redaction_partial_key_match() {
        // Keys that *contain* a sensitive pattern should also redact.
        let logger = TestLogger::new();
        logger.info_ctx("auth", "Partial match", |ctx| {
            ctx.push(("x-api-key-header".into(), "value".into()));
            ctx.push(("my_secret_value".into(), "value".into()));
            ctx.push(("safe_key".into(), "visible".into()));
        });
        let dump = logger.dump();
        assert!(dump.contains("x-api-key-header = [REDACTED]"));
        assert!(dump.contains("my_secret_value = [REDACTED]"));
        assert!(dump.contains("safe_key = visible"));
    }

    // ====================================================================
    // Correlation Model (trace_id, span_id)
    // ====================================================================

    #[test]
    fn trace_id_is_unique_per_logger() {
        let a = TestLogger::new();
        let b = TestLogger::new();
        assert_ne!(a.trace_id(), b.trace_id());
        assert_eq!(a.trace_id().len(), 32); // 16 bytes hex = 32 chars
    }

    #[test]
    fn trace_id_appears_in_jsonl_output() {
        let logger = TestLogger::new();
        logger.info("test", "hello");
        let jsonl = logger.dump_jsonl();
        let record: serde_json::Value =
            serde_json::from_str(jsonl.lines().next().unwrap()).unwrap();
        assert_eq!(record["trace_id"].as_str().unwrap(), logger.trace_id());
        assert_eq!(record["schema"], "pi.test.log.v2");
    }

    #[test]
    fn trace_id_normalized_to_placeholder() {
        let logger = TestLogger::new();
        logger.info("test", "hello");
        let jsonl = logger.dump_jsonl_normalized();
        let record: serde_json::Value =
            serde_json::from_str(jsonl.lines().next().unwrap()).unwrap();
        assert_eq!(record["trace_id"], PLACEHOLDER_TRACE_ID);
    }

    #[test]
    fn log_without_span_has_no_span_id() {
        let logger = TestLogger::new();
        logger.info("test", "no span");
        let entries = logger.entries();
        assert!(entries[0].span_id.is_none());
        assert!(entries[0].parent_span_id.is_none());
    }

    #[test]
    fn span_tags_entries_with_span_id() {
        let logger = TestLogger::new();
        let span = logger.begin_span("load_extension");
        logger.info("ext", "Loading extension");
        drop(span);

        let entries = logger.entries();
        // entries[0] = span.begin, entries[1] = "Loading extension", entries[2] = span.end
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].category, "span.begin");
        assert_eq!(entries[0].span_id.as_deref(), Some("span-1"));
        assert_eq!(entries[1].span_id.as_deref(), Some("span-1"));
        assert_eq!(entries[1].message, "Loading extension");
        assert_eq!(entries[2].category, "span.end");
        assert_eq!(entries[2].span_id.as_deref(), Some("span-1"));
    }

    #[test]
    fn nested_spans_set_parent_span_id() {
        let logger = TestLogger::new();
        let outer = logger.begin_span("outer");
        logger.info("test", "in outer");
        let inner = logger.begin_span("inner");
        logger.info("test", "in inner");
        drop(inner);
        logger.info("test", "back in outer");
        drop(outer);

        let entries = logger.entries();
        // Find "in inner" entry
        let in_inner = entries.iter().find(|e| e.message == "in inner").unwrap();
        assert_eq!(in_inner.span_id.as_deref(), Some("span-2"));
        assert_eq!(in_inner.parent_span_id.as_deref(), Some("span-1"));

        // Find "in outer" entry
        let in_outer = entries.iter().find(|e| e.message == "in outer").unwrap();
        assert_eq!(in_outer.span_id.as_deref(), Some("span-1"));
        assert!(in_outer.parent_span_id.is_none());

        // Find "back in outer" entry (after inner dropped)
        let back_in_outer = entries
            .iter()
            .find(|e| e.message == "back in outer")
            .unwrap();
        assert_eq!(back_in_outer.span_id.as_deref(), Some("span-1"));
        assert!(back_in_outer.parent_span_id.is_none());
    }

    #[test]
    fn span_guard_returns_span_id() {
        let logger = TestLogger::new();
        let span = logger.begin_span("test_span");
        assert_eq!(span.span_id(), "span-1");
        drop(span);
    }

    #[test]
    fn span_end_includes_duration() {
        let logger = TestLogger::new();
        {
            let _span = logger.begin_span("timed");
            logger.info("test", "work");
        }
        let entries = logger.entries();
        let end_entry = entries.iter().find(|e| e.category == "span.end").unwrap();
        assert!(end_entry.context.iter().any(|(k, _)| k == "duration_ms"));
    }

    #[test]
    fn span_ids_in_jsonl_output() {
        let logger = TestLogger::new();
        {
            let _span = logger.begin_span("my_op");
            logger.info("test", "inside span");
        }
        let jsonl = logger.dump_jsonl();
        let lines: Vec<serde_json::Value> = jsonl
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();

        // All records should have trace_id
        for line in &lines {
            assert!(line["trace_id"].is_string());
        }

        // span.begin record should have span_id
        let begin = lines
            .iter()
            .find(|l| l["category"] == "span.begin")
            .unwrap();
        assert_eq!(begin["span_id"], "span-1");

        // "inside span" record should have span_id
        let inside = lines
            .iter()
            .find(|l| l["message"] == "inside span")
            .unwrap();
        assert_eq!(inside["span_id"], "span-1");
    }

    #[test]
    fn span_ids_normalized_to_placeholder() {
        let logger = TestLogger::new();
        {
            let _span = logger.begin_span("op");
            logger.info("test", "spanned");
        }
        let jsonl = logger.dump_jsonl_normalized();
        let record: serde_json::Value =
            serde_json::from_str(jsonl.lines().nth(1).unwrap()).unwrap();
        assert_eq!(record["span_id"], PLACEHOLDER_SPAN_ID);
    }

    #[test]
    fn v2_jsonl_validates_against_schema() {
        let logger = TestLogger::new();
        {
            let _span = logger.begin_span("validate_test");
            logger.info("test", "validating");
        }
        let jsonl = logger.dump_jsonl();
        let errors = validate_jsonl(&jsonl);
        assert!(errors.is_empty(), "v2 JSONL validation errors: {errors:?}");
    }

    #[test]
    fn v2_schema_rejects_non_string_trace_id() {
        let record = r#"{"schema":"pi.test.log.v2","type":"log","trace_id":123,"seq":1,"ts":"x","t_ms":0,"level":"info","category":"c","message":"m"}"#;
        let err = validate_jsonl_line(record, 1).unwrap_err();
        assert_eq!(err.field, "trace_id");
    }

    #[test]
    fn v2_schema_rejects_non_string_span_id() {
        let record = r#"{"schema":"pi.test.log.v2","type":"log","trace_id":"abc","span_id":42,"seq":1,"ts":"x","t_ms":0,"level":"info","category":"c","message":"m"}"#;
        let err = validate_jsonl_line(record, 1).unwrap_err();
        assert_eq!(err.field, "span_id");
    }

    #[test]
    fn v2_schema_rejects_missing_trace_id() {
        let record = r#"{"schema":"pi.test.log.v2","type":"log","seq":1,"ts":"x","t_ms":0,"level":"info","category":"c","message":"m"}"#;
        let err = validate_jsonl_line(record, 1).unwrap_err();
        assert_eq!(err.field, "trace_id");
        assert!(err.message.contains("required field missing"));
    }

    #[test]
    fn v1_schema_still_validates() {
        // V1 records should still pass validation (backward compat).
        let record = r#"{"schema":"pi.test.log.v1","type":"log","seq":1,"ts":"2026-01-01T00:00:00.000Z","t_ms":0,"level":"info","category":"setup","message":"hello"}"#;
        assert!(validate_jsonl_line(record, 1).is_ok());
    }

    // ====================================================================
    // V2-Only Enforcement (DISC-021 / bd-38m8w)
    // ====================================================================

    #[test]
    fn v2_only_rejects_v1_log_schema() {
        let record = r#"{"schema":"pi.test.log.v1","type":"log","seq":1,"ts":"2026-01-01T00:00:00.000Z","t_ms":0,"level":"info","category":"setup","message":"hello"}"#;
        let err = validate_jsonl_line_v2_only(record, 1).unwrap_err();
        assert_eq!(err.field, "schema");
        assert!(
            err.message.contains("deprecated"),
            "Error should mention deprecated: {err}"
        );
    }

    #[test]
    fn v2_only_accepts_v2_log_schema() {
        let record = r#"{"schema":"pi.test.log.v2","type":"log","trace_id":"abc","seq":1,"ts":"x","t_ms":0,"level":"info","category":"c","message":"m"}"#;
        assert!(validate_jsonl_line_v2_only(record, 1).is_ok());
    }

    #[test]
    fn v2_only_accepts_artifact_v1_schema() {
        // Artifact schema is v1 and that's correct (not deprecated).
        let record = r#"{"schema":"pi.test.artifact.v1","type":"artifact","seq":1,"ts":"x","t_ms":0,"name":"trace","path":"/tmp/t.json"}"#;
        assert!(validate_jsonl_line_v2_only(record, 1).is_ok());
    }

    #[test]
    fn v2_only_batch_rejects_mixed_v1_v2() {
        let content = [
            r#"{"schema":"pi.test.log.v2","type":"log","trace_id":"abc","seq":1,"ts":"x","t_ms":0,"level":"info","category":"c","message":"ok"}"#,
            r#"{"schema":"pi.test.log.v1","type":"log","seq":2,"ts":"x","t_ms":0,"level":"info","category":"c","message":"bad"}"#,
            r#"{"schema":"pi.test.artifact.v1","type":"artifact","seq":3,"ts":"x","t_ms":0,"name":"a","path":"/tmp/a"}"#,
        ]
        .join("\n");

        let errors = validate_jsonl_v2_only(&content);
        assert_eq!(
            errors.len(),
            1,
            "Only the v1 log record should fail: {errors:?}"
        );
        assert_eq!(errors[0].line, 2);
    }

    #[test]
    fn v2_only_logger_output_is_compliant() {
        // Prove that TestLogger always produces v2-only compliant output.
        let logger = TestLogger::new();
        logger.info("test", "v2 check");
        logger.record_artifact("log", "/tmp/log.txt");
        let jsonl = logger.dump_jsonl();
        let errors = validate_jsonl_v2_only(&jsonl);
        assert!(
            errors.is_empty(),
            "Logger output must be v2-only compliant: {errors:?}"
        );
    }

    #[test]
    fn multiple_spans_get_sequential_ids() {
        let logger = TestLogger::new();
        let s1 = logger.begin_span("first");
        drop(s1);
        let s2 = logger.begin_span("second");
        drop(s2);
        let s3 = logger.begin_span("third");
        drop(s3);

        let entries = logger.entries();
        let begins: Vec<_> = entries
            .iter()
            .filter(|e| e.category == "span.begin")
            .collect();
        assert_eq!(begins[0].span_id.as_deref(), Some("span-1"));
        assert_eq!(begins[1].span_id.as_deref(), Some("span-2"));
        assert_eq!(begins[2].span_id.as_deref(), Some("span-3"));
    }

    #[test]
    fn context_entries_within_span_carry_span_id() {
        let logger = TestLogger::new();
        let span = logger.begin_span("with_ctx");
        logger.info_ctx("test", "ctx msg", |ctx| {
            ctx.push(("key".into(), "val".into()));
        });
        drop(span);

        let entries = logger.entries();
        let ctx_entry = entries.iter().find(|e| e.message == "ctx msg").unwrap();
        assert_eq!(ctx_entry.span_id.as_deref(), Some("span-1"));
        assert!(!ctx_entry.context.is_empty());
    }

    #[test]
    fn ci_correlation_id_reflects_environment_value() {
        let expected_ci_correlation_id = std::env::var("CI_CORRELATION_ID")
            .ok()
            .filter(|value| !value.is_empty());
        let logger = TestLogger::new();
        assert_eq!(
            logger.ci_correlation_id(),
            expected_ci_correlation_id.as_deref()
        );
        logger.info("test", "ci id reflection");
        let jsonl = logger.dump_jsonl();
        let record: serde_json::Value =
            serde_json::from_str(jsonl.lines().next().unwrap()).unwrap();
        assert_eq!(
            record
                .get("ci_correlation_id")
                .and_then(serde_json::Value::as_str),
            expected_ci_correlation_id.as_deref()
        );
    }

    #[test]
    fn ci_correlation_id_in_jsonl_passes_validation() {
        // A v2 record with ci_correlation_id as a string should validate.
        let record = r#"{"schema":"pi.test.log.v2","type":"log","trace_id":"abc","ci_correlation_id":"run-42","seq":1,"ts":"x","t_ms":0,"level":"info","category":"c","message":"m"}"#;
        assert!(validate_jsonl_line(record, 1).is_ok());
    }

    #[test]
    fn ci_correlation_id_without_value_passes_validation() {
        // A v2 record without ci_correlation_id should also validate.
        let record = r#"{"schema":"pi.test.log.v2","type":"log","trace_id":"abc","seq":1,"ts":"x","t_ms":0,"level":"info","category":"c","message":"m"}"#;
        assert!(validate_jsonl_line(record, 1).is_ok());
    }

    #[test]
    fn v2_schema_rejects_non_string_ci_correlation_id() {
        let record = r#"{"schema":"pi.test.log.v2","type":"log","trace_id":"abc","ci_correlation_id":123,"seq":1,"ts":"x","t_ms":0,"level":"info","category":"c","message":"m"}"#;
        let err = validate_jsonl_line(record, 1).unwrap_err();
        assert_eq!(err.field, "ci_correlation_id");
    }

    #[test]
    fn bead_coverage_try_new_rejects_absolute_test_file_path() {
        let test_files = vec!["/tmp/absolute-path.rs".to_string()];
        let err = BeadCoverageLink::try_new("bd-3ar8v.6.11", &test_files, EvidenceType::Unit)
            .expect_err("absolute test-file path must fail closed");
        assert!(err.contains("repo-relative"), "unexpected error: {err}");
    }

    #[test]
    fn bead_coverage_try_new_rejects_windows_absolute_test_file_path() {
        let test_files = vec!["C:/temp/absolute-path.rs".to_string()];
        let err = BeadCoverageLink::try_new("bd-3ar8v.6.11", &test_files, EvidenceType::Unit)
            .expect_err("windows absolute test-file path must fail closed");
        assert!(err.contains("repo-relative"), "unexpected error: {err}");
    }

    #[test]
    fn bead_coverage_try_new_rejects_rooted_paths_on_every_platform() {
        // gh #175: these shapes must fail closed on Unix AND Windows. The
        // guard operates on the normalized string, so this test exercises
        // the identical code path a Windows host takes.
        for candidate in [
            "/tmp/absolute-path.rs",      // POSIX-absolute (the reported gap)
            "\\tmp\\absolute-path.rs",    // rooted backslash form
            "//server/share/path.rs",     // forward-slash UNC
            "\\\\server\\share\\path.rs", // backslash UNC
            "C:\\temp\\absolute-path.rs", // drive-absolute, backslashes
            "C:temp/drive-relative.rs",   // drive-relative (no root)
            "c:onefile.rs",               // lowercase drive-relative
        ] {
            let test_files = vec![candidate.to_string()];
            let err = BeadCoverageLink::try_new("bd-3ar8v.6.11", &test_files, EvidenceType::Unit)
                .expect_err(&format!("non-portable path must fail closed: {candidate}"));
            assert!(err.contains("repo-relative"), "unexpected error: {err}");
        }
    }

    #[test]
    fn bead_coverage_try_new_accepts_backslash_relative_path() {
        // Planted negative for the gh #175 tightening: a genuinely
        // repo-relative path written with backslashes must still be accepted
        // (normalization maps it to forward slashes).
        let test_files = vec!["src\\lib.rs".to_string()];
        let link = BeadCoverageLink::try_new("bd-3ar8v.6.11", &test_files, EvidenceType::Unit)
            .expect("backslash repo-relative path must remain accepted");
        assert_eq!(link.test_files, vec!["src/lib.rs".to_string()]);
    }

    #[test]
    fn bead_coverage_try_new_rejects_empty_test_files() {
        let test_files: Vec<String> = Vec::new();
        let err = BeadCoverageLink::try_new("bd-3ar8v.6.11", &test_files, EvidenceType::Unit)
            .expect_err("empty test_files must fail closed");
        assert!(err.contains("at least one"), "unexpected error: {err}");
    }

    #[test]
    fn bead_coverage_validate_path_hygiene_rejects_empty_test_files() {
        let link = BeadCoverageLink {
            schema: BEAD_COVERAGE_SCHEMA_V1,
            bead_id: "bd-3ar8v.6.11".to_string(),
            test_files: Vec::new(),
            log_artifacts: Vec::new(),
            evidence_type: EvidenceType::Unit,
        };
        let err = link
            .validate_path_hygiene()
            .expect_err("empty test_files must fail closed");
        assert!(err.contains("at least one"), "unexpected error: {err}");
    }

    #[test]
    fn bead_coverage_try_new_rejects_dot_path_after_normalization() {
        let test_files = vec!["./".to_string()];
        let err = BeadCoverageLink::try_new("bd-3ar8v.6.11", &test_files, EvidenceType::Unit)
            .expect_err("dot-path normalization to empty path must fail closed");
        assert!(
            err.contains("repo-relative file path"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn bead_coverage_try_new_rejects_single_dot_path() {
        let test_files = vec![".".to_string()];
        let err = BeadCoverageLink::try_new("bd-3ar8v.6.11", &test_files, EvidenceType::Unit)
            .expect_err("single-dot path must fail closed");
        assert!(
            err.contains("repo-relative file path"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn bead_coverage_validate_path_hygiene_rejects_parent_traversal_log_artifact() {
        let test_files = vec!["tests/ci_full_suite_gate.rs".to_string()];
        let mut link = BeadCoverageLink::new("bd-3ar8v.6.11", &test_files, EvidenceType::Unit);
        link.log_artifacts = vec!["../secrets/leak.jsonl".to_string()];

        let err = link
            .validate_path_hygiene()
            .expect_err("parent traversal must fail closed");
        assert!(err.contains(".."), "unexpected error: {err}");
    }

    #[test]
    fn bead_coverage_validate_path_hygiene_accepts_repo_relative_paths() {
        let test_files = vec!["tests/ci_full_suite_gate.rs".to_string()];
        let mut link = BeadCoverageLink::new("bd-3ar8v.6.11", &test_files, EvidenceType::Unit);
        link.log_artifacts = vec![
            "tests/full_suite_gate/full_suite_events.jsonl".to_string(),
            "tests/full_suite_gate/certification_events.jsonl".to_string(),
        ];

        link.validate_path_hygiene()
            .expect("repo-relative evidence paths should pass");

        let json = link.to_jsonl_line();
        let parsed: serde_json::Value =
            serde_json::from_str(&json).expect("BeadCoverageLink JSON should deserialize");
        assert_eq!(parsed["schema"], BEAD_COVERAGE_SCHEMA_V1);
        assert_eq!(parsed["bead_id"], "bd-3ar8v.6.11");
    }

    #[test]
    fn bead_coverage_try_new_rejects_duplicate_test_files_after_normalization() {
        let test_files = vec![
            "tests/ci_full_suite_gate.rs".to_string(),
            " tests/ci_full_suite_gate.rs ".to_string(),
        ];
        let err = BeadCoverageLink::try_new("bd-3ar8v.6.11", &test_files, EvidenceType::Unit)
            .expect_err("duplicate normalized test-files must fail closed");
        assert!(err.contains("duplicates"), "unexpected error: {err}");
    }

    #[test]
    fn bead_coverage_validate_path_hygiene_rejects_duplicate_log_artifacts() {
        let test_files = vec!["tests/ci_full_suite_gate.rs".to_string()];
        let mut link = BeadCoverageLink::new("bd-3ar8v.6.11", &test_files, EvidenceType::Unit);
        link.log_artifacts = vec![
            "tests/full_suite_gate/full_suite_events.jsonl".to_string(),
            " tests/full_suite_gate/full_suite_events.jsonl ".to_string(),
        ];

        let err = link
            .validate_path_hygiene()
            .expect_err("duplicate normalized log artifacts must fail closed");
        assert!(err.contains("duplicates"), "unexpected error: {err}");
    }

    #[test]
    fn bead_coverage_try_new_rejects_duplicate_test_files_with_separator_variants() {
        let test_files = vec![
            "./tests\\ci_full_suite_gate.rs".to_string(),
            "tests/ci_full_suite_gate.rs".to_string(),
        ];
        let err = BeadCoverageLink::try_new("bd-3ar8v.6.11", &test_files, EvidenceType::Unit)
            .expect_err("separator/./ variants must fail closed as duplicates");
        assert!(err.contains("duplicates"), "unexpected error: {err}");
    }

    #[test]
    fn bead_coverage_validate_path_hygiene_rejects_windows_parent_traversal_log_artifact() {
        let test_files = vec!["tests/ci_full_suite_gate.rs".to_string()];
        let mut link = BeadCoverageLink::new("bd-3ar8v.6.11", &test_files, EvidenceType::Unit);
        link.log_artifacts = vec!["tests\\..\\secrets\\leak.jsonl".to_string()];

        let err = link
            .validate_path_hygiene()
            .expect_err("windows-style parent traversal must fail closed");
        assert!(err.contains(".."), "unexpected error: {err}");
    }

    #[test]
    fn bead_coverage_try_new_normalizes_repo_relative_paths() {
        let test_files = vec!["./tests\\ci_full_suite_gate.rs".to_string()];
        let link = BeadCoverageLink::try_new("bd-3ar8v.6.11", &test_files, EvidenceType::Unit)
            .expect("normalized repo-relative path should be accepted");
        assert_eq!(link.test_files, vec!["tests/ci_full_suite_gate.rs"]);
    }
}
