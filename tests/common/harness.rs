//! Test harness for consistent setup/teardown and auto-logging.
//!
//! The `TestHarness` provides:
//! - A temporary directory for test files
//! - A test logger for detailed tracing
//! - Automatic log dump on test failure (panic)
//! - Timing information for performance analysis
//!
//! # Example
//!
//! ```ignore
//! #[test]
//! fn test_something() {
//!     let harness = TestHarness::new("test_something");
//!
//!     harness.log().info("setup", "Creating test environment");
//!     let test_file = harness.temp_path("data.txt");
//!     std::fs::write(&test_file, "test content").unwrap();
//!
//!     harness.log().info_ctx("action", "Processing file", |ctx| {
//!         ctx.push(("path".into(), test_file.display().to_string()));
//!     });
//!
//!     // On test failure, detailed logs are automatically printed
//!     assert!(std::fs::read_to_string(&test_file).unwrap().contains("test"));
//! }
//! ```

#![allow(dead_code)]

use super::logging::{LogLevel, TestLogger};
use futures::{FutureExt, StreamExt, pin_mut};
use pi::auth::AuthStorage;
use pi::config::Config;
use pi::http::client::Client;
use pi::model::{Message, StreamEvent, ThinkingLevel, Usage, UserContent, UserMessage};
use pi::models::{ModelEntry, ModelRegistry, default_models_path};
use pi::provider::{Context, Provider, StreamOptions};
use pi::provider_metadata::provider_auth_env_keys;
use pi::providers::anthropic::AnthropicProvider;
use pi::providers::gemini::GeminiProvider;
use pi::providers::openai::OpenAIProvider;
use pi::providers::openai_responses::OpenAIResponsesProvider;
use pi::providers::{normalize_openai_base, normalize_openai_responses_base};
use pi::vcr::{Cassette, VcrMode, VcrRecorder};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::env;
use std::fmt::Write as _;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tempfile::TempDir;

/// Test harness providing temp directories, logging, and cleanup.
pub struct TestHarness {
    /// Test name for identification in logs.
    name: String,
    /// Temporary directory for test files.
    temp_dir: TempDir,
    /// Canonicalized path (resolves macOS `/var` → `/private/var` symlinks).
    canonical_dir: PathBuf,
    /// Test logger for detailed tracing.
    logger: Arc<TestLogger>,
    /// Whether to use colored output.
    use_colors: bool,
}

#[allow(dead_code)]
impl TestHarness {
    /// Create a new test harness with the given test name.
    ///
    /// The test name is used to identify the test in log output.
    pub fn new(name: impl Into<String>) -> Self {
        let name = name.into();
        let temp_dir = TempDir::new().expect("Failed to create temp directory");
        let canonical_dir = pi::extensions::strip_unc_prefix(
            std::fs::canonicalize(temp_dir.path())
                .unwrap_or_else(|_| temp_dir.path().to_path_buf()),
        );
        let logger = Arc::new(TestLogger::new());
        logger.set_test_name(&name);
        logger.set_normalization_root(&canonical_dir);

        logger
            .as_ref()
            .info("harness", format!("Test '{name}' started"));
        logger
            .as_ref()
            .info_ctx("harness", "Temp directory created", |ctx| {
                ctx.push(("path".into(), canonical_dir.display().to_string()));
            });

        Self {
            name,
            temp_dir,
            canonical_dir,
            logger,
            use_colors: true,
        }
    }

    /// Create a harness without colored output.
    pub fn new_plain(name: impl Into<String>) -> Self {
        let mut harness = Self::new(name);
        harness.use_colors = false;
        harness
    }

    /// Get a reference to the test logger.
    pub fn log(&self) -> &TestLogger {
        self.logger.as_ref()
    }

    /// Clone the underlying logger for use from helper threads.
    pub fn logger_arc(&self) -> Arc<TestLogger> {
        Arc::clone(&self.logger)
    }

    /// Get the path to the temporary directory (canonicalized).
    pub fn temp_dir(&self) -> &Path {
        &self.canonical_dir
    }

    /// Get a path within the temporary directory (canonicalized).
    ///
    /// This is a convenience method that joins the given path to the temp directory.
    pub fn temp_path(&self, path: impl AsRef<Path>) -> PathBuf {
        self.canonical_dir.join(path)
    }

    /// Create a file in the temp directory with the given content.
    ///
    /// Returns the full path to the created file.
    pub fn create_file(&self, name: impl AsRef<Path>, content: impl AsRef<[u8]>) -> PathBuf {
        let path = self.temp_path(name);

        // Create parent directories if needed
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("Failed to create parent directories");
        }

        let content_ref = content.as_ref();
        std::fs::write(&path, content_ref).expect("Failed to create test file");

        self.logger.info_ctx("harness", "Created test file", |ctx| {
            ctx.push(("path".into(), path.display().to_string()));
            ctx.push(("size".into(), format!("{} bytes", content_ref.len())));
        });

        path
    }

    /// Create a directory in the temp directory.
    ///
    /// Returns the full path to the created directory.
    pub fn create_dir(&self, name: impl AsRef<Path>) -> PathBuf {
        let path = self.temp_path(name);
        std::fs::create_dir_all(&path).expect("Failed to create test directory");

        self.logger
            .info_ctx("harness", "Created test directory", |ctx| {
                ctx.push(("path".into(), path.display().to_string()));
            });

        path
    }

    /// Read a file from the temp directory.
    pub fn read_file(&self, name: impl AsRef<Path>) -> String {
        let path = self.temp_path(name);
        let content = std::fs::read_to_string(&path).expect("Failed to read test file");

        self.logger.debug_ctx("harness", "Read test file", |ctx| {
            ctx.push(("path".into(), path.display().to_string()));
            ctx.push(("size".into(), format!("{} bytes", content.len())));
        });

        content
    }

    /// Check if a file exists in the temp directory.
    pub fn file_exists(&self, name: impl AsRef<Path>) -> bool {
        self.temp_path(name).exists()
    }

    /// Log a test section start (useful for organizing multi-phase tests).
    pub fn section(&self, name: &str) {
        self.logger.info("section", format!("=== {name} ==="));
    }

    /// Log an assertion about to happen (useful for debugging which assertion failed).
    pub fn assert_log(&self, description: &str) {
        self.logger.debug("assert", description);
    }

    /// Get the test name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Get elapsed time since harness creation.
    pub fn elapsed(&self) -> std::time::Duration {
        self.logger.elapsed()
    }

    /// Return logs as JSONL for machine assertions and triage workflows.
    pub fn dump_logs(&self) -> String {
        self.logger.dump_jsonl()
    }

    /// Compatibility helper for older tests.
    pub fn has_artifacts(&self) -> bool {
        self.logger.has_artifacts()
    }

    /// Compatibility helper for older tests.
    pub fn dump_artifact_index(&self) -> String {
        self.logger.dump_artifact_index_jsonl()
    }

    /// Compatibility helper for older tests.
    pub fn info(&self, message: impl Into<String>) {
        self.logger.info("test", message);
    }

    /// Compatibility helper for older tests.
    pub fn info_ctx(&self, message: impl Into<String>, fields: &[(&str, &str)]) {
        let message = message.into();
        self.logger.info_ctx("test", message, |ctx| {
            for (key, value) in fields {
                ctx.push(((*key).to_string(), (*value).to_string()));
            }
        });
    }

    /// Record an artifact for this test.
    pub fn record_artifact(&self, name: impl Into<String>, path: impl AsRef<Path>) {
        self.logger.as_ref().record_artifact(name, path);
    }

    /// Write test logs as JSONL.
    pub fn write_jsonl_logs(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
        self.logger.as_ref().write_jsonl_to_path(path)
    }

    /// Write normalized test logs as JSONL.
    pub fn write_jsonl_logs_normalized(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
        self.logger.as_ref().write_jsonl_normalized_to_path(path)
    }

    /// Write artifact index as JSONL.
    pub fn write_artifact_index_jsonl(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
        self.logger
            .as_ref()
            .write_artifact_index_jsonl_to_path(path)
    }

    /// Write normalized artifact index as JSONL.
    pub fn write_artifact_index_jsonl_normalized(
        &self,
        path: impl AsRef<Path>,
    ) -> std::io::Result<()> {
        self.logger
            .as_ref()
            .write_artifact_index_jsonl_normalized_to_path(path)
    }

    /// Derive a stable per-test seed for deterministic harness behavior.
    ///
    /// This is intended for tests and harness utilities, not cryptography.
    pub fn deterministic_seed(&self) -> u64 {
        let mut hasher = Sha256::new();
        hasher.update(self.name.as_bytes());
        let digest = hasher.finalize();
        u64::from_le_bytes(
            digest[..8]
                .try_into()
                .expect("sha256 digest contains at least 8 bytes"),
        )
    }

    /// Create (and return) a directory inside the harness temp dir for a scenario.
    ///
    /// This provides stable, isolated workspaces per scenario in E2E/conformance tests.
    pub fn create_workspace(&self, name: impl AsRef<Path>) -> PathBuf {
        self.create_dir(name)
    }

    /// Start a local mock HTTP server for deterministic, offline tests.
    pub fn start_mock_http_server(&self) -> MockHttpServer {
        MockHttpServer::start(self.logger_arc())
    }

    /// Build an isolated Pi environment rooted inside the harness temp directory.
    pub fn isolated_pi_env(&self) -> TestEnv {
        let env_root = self.temp_path("pi-env");
        let _ = std::fs::create_dir_all(&env_root);

        let mut env = TestEnv::new();
        env.set(
            "PI_CODING_AGENT_DIR",
            env_root.join("agent").display().to_string(),
        );
        env.set(
            "PI_CONFIG_PATH",
            env_root.join("settings.json").display().to_string(),
        );
        env.set(
            "PI_SESSIONS_DIR",
            env_root.join("sessions").display().to_string(),
        );
        env.set(
            "PI_PACKAGE_DIR",
            env_root.join("packages").display().to_string(),
        );
        env
    }
}

impl TestLogger {
    /// Log a debug entry with context.
    pub fn debug_ctx<F>(&self, category: &str, message: impl Into<String>, f: F)
    where
        F: FnOnce(&mut Vec<(String, String)>),
    {
        self.with_context(LogLevel::Debug, category, message, f);
    }
}

impl Drop for TestHarness {
    fn drop(&mut self) {
        // Log completion
        self.logger
            .as_ref()
            .info_ctx("harness", "Test completing", |ctx| {
                ctx.push((
                    "elapsed".into(),
                    format!("{:.3}s", self.elapsed().as_secs_f64()),
                ));
            });

        if let Ok(path) = env::var("TEST_LOG_JSONL_PATH") {
            if let Err(err) = self.logger.as_ref().write_jsonl_to_path(&path) {
                eprintln!("Failed to write JSONL test log to {path}: {err}");
            }
            let normalized_path = normalized_jsonl_path(Path::new(&path));
            if let Err(err) = self
                .logger
                .as_ref()
                .write_jsonl_normalized_to_path(&normalized_path)
            {
                eprintln!(
                    "Failed to write normalized JSONL test log to {}: {err}",
                    normalized_path.display()
                );
            }
        }

        if let Ok(path) = env::var("TEST_ARTIFACT_INDEX_PATH") {
            if let Err(err) = self
                .logger
                .as_ref()
                .write_artifact_index_jsonl_to_path(&path)
            {
                eprintln!("Failed to write artifact index JSONL to {path}: {err}");
            }
            let normalized_path = normalized_jsonl_path(Path::new(&path));
            if let Err(err) = self
                .logger
                .as_ref()
                .write_artifact_index_jsonl_normalized_to_path(&normalized_path)
            {
                eprintln!(
                    "Failed to write normalized artifact index JSONL to {}: {err}",
                    normalized_path.display()
                );
            }
        }

        // Dump logs if we're panicking (test failure)
        if std::thread::panicking() {
            let header = format!("\n=== TEST FAILED: {} ===\n", self.name);
            if self.use_colors {
                eprint!("\x1b[1;31m{header}\x1b[0m");
                eprint!("{}", self.logger.as_ref().dump_colored());
            } else {
                eprint!("{header}");
                eprint!("{}", self.logger.as_ref().dump());
            }
            if self.logger.as_ref().has_artifacts() {
                eprintln!("=== ARTIFACTS ===");
                eprint!("{}", self.logger.as_ref().dump_artifacts());
                eprintln!("=== END ARTIFACTS ===");
            }
            eprintln!("=== END LOGS ===\n");

            if let Ok(path) = env::var("TEST_LOG_PATH")
                && let Err(err) = self.logger.as_ref().write_dump_to_path(&path)
            {
                eprintln!("Failed to write test log to {path}: {err}");
            }
        }
    }
}

fn normalized_jsonl_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("log.jsonl");
    file_name.strip_suffix(".jsonl").map_or_else(
        || path.with_file_name(format!("{file_name}.normalized.jsonl")),
        |stripped| path.with_file_name(format!("{stripped}.normalized.jsonl")),
    )
}

/// Builder for configuring test harnesses.
pub struct TestHarnessBuilder {
    name: String,
    use_colors: bool,
    min_log_level: LogLevel,
}

impl TestHarnessBuilder {
    /// Create a new builder with the given test name.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            use_colors: true,
            min_log_level: LogLevel::Debug,
        }
    }

    /// Disable colored output.
    pub const fn no_colors(mut self) -> Self {
        self.use_colors = false;
        self
    }

    /// Set minimum log level.
    pub const fn min_level(mut self, level: LogLevel) -> Self {
        self.min_log_level = level;
        self
    }

    /// Build the test harness.
    pub fn build(self) -> TestHarness {
        let temp_dir = TempDir::new().expect("Failed to create temp directory");
        let canonical_dir = pi::extensions::strip_unc_prefix(
            std::fs::canonicalize(temp_dir.path())
                .unwrap_or_else(|_| temp_dir.path().to_path_buf()),
        );
        let logger = Arc::new(TestLogger::with_min_level(self.min_log_level));
        let name = self.name;
        logger.set_test_name(&name);
        logger.set_normalization_root(&canonical_dir);

        logger
            .as_ref()
            .info("harness", format!("Test '{name}' started"));
        logger
            .as_ref()
            .info_ctx("harness", "Temp directory created", |ctx| {
                ctx.push(("path".into(), canonical_dir.display().to_string()));
            });

        TestHarness {
            name,
            temp_dir,
            canonical_dir,
            logger,
            use_colors: self.use_colors,
        }
    }
}

// ============================================================================
// Deterministic Environment Helpers
// ============================================================================

/// A simple environment variable map with stable ordering for logging.
#[derive(Debug, Clone, Default)]
pub struct TestEnv {
    vars: std::collections::BTreeMap<String, String>,
}

impl TestEnv {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            vars: std::collections::BTreeMap::new(),
        }
    }

    pub fn set(&mut self, key: impl Into<String>, value: impl Into<String>) -> &mut Self {
        self.vars.insert(key.into(), value.into());
        self
    }

    #[must_use]
    pub const fn vars(&self) -> &std::collections::BTreeMap<String, String> {
        &self.vars
    }

    /// Log the environment with secret redaction (handled by `TestLogger` key redaction).
    pub fn log(&self, logger: &TestLogger, category: &str, message: &str) {
        logger.info_ctx(category, message, |ctx| {
            for (key, value) in &self.vars {
                ctx.push((key.clone(), value.clone()));
            }
        });
    }

    pub fn apply_to(&self, command: &mut std::process::Command) {
        command.envs(self.vars.clone());
    }
}

// ============================================================================
// Mock HTTP Server (Offline deterministic test infra)
// ============================================================================

#[derive(Debug, Clone)]
pub struct MockHttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl MockHttpResponse {
    #[must_use]
    pub fn text(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            headers: vec![("Content-Type".to_string(), "text/plain".to_string())],
            body: body.into().into_bytes(),
        }
    }

    #[must_use]
    pub fn json(status: u16, value: &serde_json::Value) -> Self {
        Self {
            status,
            headers: vec![("Content-Type".to_string(), "application/json".to_string())],
            body: serde_json::to_vec(value).unwrap_or_default(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct MockHttpRequest {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RouteKey {
    method: String,
    path: String,
}

pub struct MockHttpServer {
    addr: SocketAddr,
    routes: Arc<Mutex<std::collections::HashMap<RouteKey, MockHttpResponse>>>,
    route_queues: Arc<
        Mutex<std::collections::HashMap<RouteKey, std::collections::VecDeque<MockHttpResponse>>>,
    >,
    requests: Arc<Mutex<Vec<MockHttpRequest>>>,
    shutdown: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    logger: Arc<TestLogger>,
}

impl MockHttpServer {
    #[must_use]
    pub fn start(logger: Arc<TestLogger>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock http listener");
        let addr = listener.local_addr().expect("mock http listener addr");
        listener
            .set_nonblocking(true)
            .expect("set mock http listener nonblocking");

        let routes = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let route_queues = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let shutdown = Arc::new(AtomicBool::new(false));

        let thread_routes = Arc::clone(&routes);
        let thread_queues = Arc::clone(&route_queues);
        let thread_requests = Arc::clone(&requests);
        let thread_shutdown = Arc::clone(&shutdown);
        let thread_logger = Arc::clone(&logger);

        let (ready_tx, ready_rx) = mpsc::channel::<()>();

        let join = thread::spawn(move || {
            let _ = ready_tx.send(());
            let mut scratch = [0u8; 16 * 1024];

            while !thread_shutdown.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, peer)) => {
                        if let Err(err) = handle_connection(
                            &mut stream,
                            peer,
                            &thread_routes,
                            &thread_queues,
                            &thread_requests,
                            &thread_logger,
                            &mut scratch,
                        ) {
                            thread_logger.error_ctx(
                                "mock_http",
                                "Connection handler error",
                                |ctx| {
                                    ctx.push(("peer".into(), peer.to_string()));
                                    ctx.push(("error".into(), err.to_string()));
                                },
                            );
                        }
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(err) => {
                        thread_logger.error_ctx("mock_http", "Listener accept error", |ctx| {
                            ctx.push(("error".into(), err.to_string()));
                        });
                        break;
                    }
                }
            }
        });

        let _ = ready_rx.recv();

        logger.info_ctx("mock_http", "Mock HTTP server started", |ctx| {
            ctx.push(("addr".into(), addr.to_string()));
        });

        Self {
            addr,
            routes,
            route_queues,
            requests,
            shutdown,
            join: Some(join),
            logger,
        }
    }

    #[must_use]
    pub const fn addr(&self) -> SocketAddr {
        self.addr
    }

    #[must_use]
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn add_route(&self, method: &str, path: &str, response: MockHttpResponse) {
        let key = RouteKey {
            method: method.trim().to_ascii_uppercase(),
            path: path.to_string(),
        };
        self.routes.lock().unwrap().insert(key, response);
    }

    /// Add a queue of responses for a route. Each request pops the front.
    /// When the queue is exhausted, falls back to the static route (if any).
    pub fn add_route_queue(&self, method: &str, path: &str, responses: Vec<MockHttpResponse>) {
        let key = RouteKey {
            method: method.trim().to_ascii_uppercase(),
            path: path.to_string(),
        };
        self.route_queues
            .lock()
            .unwrap()
            .insert(key, std::collections::VecDeque::from(responses));
    }

    #[must_use]
    pub fn requests(&self) -> Vec<MockHttpRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for MockHttpServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);

        // Best-effort: poke the listener to unblock accept loops on some platforms.
        if let Ok(stream) = TcpStream::connect(self.addr) {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }

        if let Some(join) = self.join.take() {
            let _ = join.join();
        }

        self.logger
            .info_ctx("mock_http", "Mock HTTP server stopped", |ctx| {
                ctx.push(("addr".into(), self.addr.to_string()));
                ctx.push((
                    "requests".into(),
                    self.requests.lock().unwrap().len().to_string(),
                ));
            });
    }
}

fn handle_connection(
    stream: &mut TcpStream,
    peer: SocketAddr,
    routes: &Arc<Mutex<std::collections::HashMap<RouteKey, MockHttpResponse>>>,
    route_queues: &Arc<
        Mutex<std::collections::HashMap<RouteKey, std::collections::VecDeque<MockHttpResponse>>>,
    >,
    requests: &Arc<Mutex<Vec<MockHttpRequest>>>,
    logger: &TestLogger,
    scratch: &mut [u8],
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(std::time::Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(std::time::Duration::from_secs(2)))?;

    let mut buf = Vec::with_capacity(8192);
    let header_end = loop {
        if let Some(pos) = find_double_crlf(&buf) {
            break pos;
        }
        match stream.read(scratch) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "connection closed before request headers",
                ));
            }
            Ok(n) => buf.extend_from_slice(&scratch[..n]),
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                // Transient EAGAIN (macOS); retry after a short sleep.
                std::thread::sleep(std::time::Duration::from_millis(1));
                continue;
            }
            Err(err) => return Err(err),
        }
        if buf.len() > 64 * 1024 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "request headers too large",
            ));
        }
    };

    let header_bytes = &buf[..header_end];
    let mut body_bytes = buf[(header_end + 4)..].to_vec();

    let header_text = std::str::from_utf8(header_bytes).map_err(|err| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, format!("utf-8: {err}"))
    })?;

    let mut lines = header_text.split("\r\n");
    let request_line = lines.next().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "missing request line")
    })?;

    let (method, path, _version) = parse_request_line(request_line)?;

    let mut headers = Vec::new();
    let mut content_length: usize = 0;

    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim().to_string();
            let value = value.trim().to_string();
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.parse().unwrap_or(0);
            }
            headers.push((name, value));
        }
    }

    while body_bytes.len() < content_length {
        let remaining = content_length - body_bytes.len();
        let to_read = remaining.min(scratch.len());
        let n = stream.read(&mut scratch[..to_read])?;
        if n == 0 {
            break;
        }
        body_bytes.extend_from_slice(&scratch[..n]);
    }

    let request = MockHttpRequest {
        method: method.clone(),
        path: path.clone(),
        headers: headers.clone(),
        body: body_bytes,
    };

    requests.lock().unwrap().push(request.clone());

    logger.info_ctx("mock_http", "Request received", |ctx| {
        ctx.push(("peer".into(), peer.to_string()));
        ctx.push(("method".into(), request.method.clone()));
        ctx.push(("path".into(), request.path.clone()));
        ctx.push(("body_len".into(), request.body.len().to_string()));
        for (name, value) in &request.headers {
            ctx.push((
                format!("header.{}", name.to_ascii_lowercase()),
                value.clone(),
            ));
        }
    });

    let route_key = RouteKey {
        method: method.clone(),
        path: path.clone(),
    };
    let response = route_queues
        .lock()
        .unwrap()
        .get_mut(&route_key)
        .and_then(std::collections::VecDeque::pop_front)
        .or_else(|| routes.lock().unwrap().get(&route_key).cloned())
        .or_else(|| {
            // Fallback: match on the bare path when the request carries a
            // query string. Exact matches (above) always win; no registered
            // route contains a query, so this only widens.
            path.split_once('?').and_then(|(bare, _)| {
                let bare_key = RouteKey {
                    method: method.clone(),
                    path: bare.to_string(),
                };
                route_queues
                    .lock()
                    .unwrap()
                    .get_mut(&bare_key)
                    .and_then(std::collections::VecDeque::pop_front)
                    .or_else(|| routes.lock().unwrap().get(&bare_key).cloned())
            })
        })
        .unwrap_or_else(|| MockHttpResponse::text(404, "not found"));

    write_response(stream, &response)?;
    Ok(())
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn parse_request_line(line: &str) -> std::io::Result<(String, String, String)> {
    let mut parts = line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "missing method"))?;
    let path = parts
        .next()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "missing path"))?;
    let version = parts
        .next()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "missing version"))?;

    Ok((
        method.trim().to_ascii_uppercase(),
        path.trim().to_string(),
        version.trim().to_string(),
    ))
}

const fn reason_phrase(status: u16) -> &'static str {
    match status {
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "OK",
    }
}

fn write_response(stream: &mut TcpStream, response: &MockHttpResponse) -> std::io::Result<()> {
    let mut head = String::new();
    let _ = write!(
        &mut head,
        "HTTP/1.1 {} {}\r\n",
        response.status,
        reason_phrase(response.status)
    );

    let mut has_content_type = false;
    for (name, value) in &response.headers {
        if name.eq_ignore_ascii_case("content-type") {
            has_content_type = true;
        }
        let _ = write!(&mut head, "{name}: {value}\r\n");
    }
    if !has_content_type {
        let _ = write!(&mut head, "Content-Type: text/plain\r\n");
    }

    let _ = write!(&mut head, "Content-Length: {}\r\n", response.body.len());
    let _ = write!(&mut head, "Connection: close\r\n");
    head.push_str("\r\n");

    stream.write_all(head.as_bytes())?;
    stream.write_all(&response.body)?;
    stream.flush()?;
    Ok(())
}

// ============================================================================
// Live Provider E2E Harness Helpers
// ============================================================================

pub const LIVE_E2E_GATE_ENV: &str = "CI_E2E_TESTS";
pub const LIVE_E2E_TIMEOUT: Duration = Duration::from_secs(30);
pub const LIVE_SHORT_PROMPT: &str = "Say just the word hello.";
pub const LIVE_E2E_EXECUTION_MODE: &str = "live_record";
pub const LIVE_E2E_TRACE_ORIGIN: &str = "vcr_last_interaction";
pub const LIVE_E2E_REPLAY_BOUNDARY: &str = "live_request_then_vcr_trace_extract";
pub const LIVE_E2E_MAX_ATTEMPTS: usize = 3;
pub const LIVE_E2E_RETRYABLE_HTTP_STATUS: [u16; 7] = [408, 429, 500, 502, 503, 504, 529];
pub const LIVE_E2E_RETRY_BACKOFF_MS: [u64; 2] = [500, 1_500];

const LIVE_E2E_REDACTION_KEY_FRAGMENTS: [&str; 10] = [
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
const LIVE_E2E_REDACTED_VALUE: &str = "[REDACTED]";

#[must_use]
fn provider_api_key_env_vars(provider: &str) -> &'static [&'static str] {
    provider_auth_env_keys(provider)
}

#[must_use]
fn is_live_sensitive_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    LIVE_E2E_REDACTION_KEY_FRAGMENTS
        .iter()
        .any(|fragment| lower.contains(fragment))
}

#[must_use]
fn redact_sensitive_header_pairs(headers: Vec<(String, String)>) -> Vec<(String, String)> {
    headers
        .into_iter()
        .map(|(key, value)| {
            if is_live_sensitive_key(&key) {
                (key, LIVE_E2E_REDACTED_VALUE.to_string())
            } else {
                (key, value)
            }
        })
        .collect()
}

#[must_use]
fn is_retryable_status(status: u16) -> bool {
    LIVE_E2E_RETRYABLE_HTTP_STATUS.contains(&status)
}

#[must_use]
fn is_retryable_error(error_message: Option<&str>) -> bool {
    let Some(error_message) = error_message else {
        return false;
    };
    let lower = error_message.to_ascii_lowercase();
    [
        "timed out",
        "timeout",
        "temporarily unavailable",
        "connection reset",
        "connection refused",
        "econnreset",
        "econnrefused",
        "eagain",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

#[must_use]
fn retry_backoff_for_attempt(attempt: usize) -> Duration {
    let idx = attempt
        .saturating_sub(1)
        .min(LIVE_E2E_RETRY_BACKOFF_MS.len().saturating_sub(1));
    Duration::from_millis(LIVE_E2E_RETRY_BACKOFF_MS[idx])
}

#[must_use]
fn should_retry_live_attempt(
    attempt: usize,
    response_status: Option<u16>,
    error_message: Option<&str>,
) -> bool {
    if attempt >= LIVE_E2E_MAX_ATTEMPTS {
        return false;
    }
    response_status.is_some_and(is_retryable_status) || is_retryable_error(error_message)
}

#[derive(Debug, Clone, Copy)]
pub struct LiveProviderTarget {
    pub provider: &'static str,
    pub model_env_var: &'static str,
    pub preferred_models: &'static [&'static str],
    pub prompt: &'static str,
}

impl LiveProviderTarget {
    #[must_use]
    pub const fn new(
        provider: &'static str,
        model_env_var: &'static str,
        preferred_models: &'static [&'static str],
        prompt: &'static str,
    ) -> Self {
        Self {
            provider,
            model_env_var,
            preferred_models,
            prompt,
        }
    }
}

#[derive(Debug, Clone)]
pub struct LiveE2eRegistry {
    pub agent_dir: PathBuf,
    pub models_path: PathBuf,
    pub auth_path: PathBuf,
    pub auth: AuthStorage,
    pub registry: ModelRegistry,
}

impl LiveE2eRegistry {
    pub fn load(logger: &TestLogger) -> Result<Self, String> {
        let agent_dir = Config::global_dir();
        let models_path = default_models_path(&agent_dir);
        let auth_path = Config::auth_path();
        let auth = AuthStorage::load(auth_path.clone())
            .map_err(|err| format!("load auth storage {}: {err}", auth_path.display()))?;
        let registry = ModelRegistry::load(&auth, Some(models_path.clone()));

        logger.info_ctx("live_e2e", "Loaded live provider registry", |ctx| {
            ctx.push(("agent_dir".into(), agent_dir.display().to_string()));
            ctx.push(("models_path".into(), models_path.display().to_string()));
            ctx.push(("auth_path".into(), auth_path.display().to_string()));
            ctx.push(("models".into(), registry.models().len().to_string()));
            ctx.push((
                "available".into(),
                registry.get_available().len().to_string(),
            ));
        });

        if let Some(err) = registry.error() {
            logger.warn("live_e2e", format!("models.json load warning: {err}"));
        }

        Ok(Self {
            agent_dir,
            models_path,
            auth_path,
            auth,
            registry,
        })
    }

    #[must_use]
    pub fn resolve_api_key_with_source(&self, entry: &ModelEntry) -> Option<(String, String)> {
        for env_var in provider_api_key_env_vars(&entry.model.provider) {
            if let Ok(value) = env::var(env_var) {
                let value = value.trim();
                if !value.is_empty() {
                    return Some((value.to_string(), format!("env:{env_var}")));
                }
            }
        }

        if let Some(value) = self.auth.api_key(&entry.model.provider) {
            let value = value.trim();
            if !value.is_empty() {
                return Some((value.to_string(), "auth_store".to_string()));
            }
        }

        if let Some(value) = &entry.api_key {
            let value = value.trim();
            if !value.is_empty() {
                return Some((value.to_string(), "models_json".to_string()));
            }
        }

        None
    }

    #[must_use]
    pub fn resolve_api_key(&self, entry: &ModelEntry) -> Option<String> {
        self.resolve_api_key_with_source(entry)
            .map(|(api_key, _source)| api_key)
    }

    #[must_use]
    pub fn select_entry(
        &self,
        target: &LiveProviderTarget,
        requested_model: Option<&str>,
    ) -> Option<ModelEntry> {
        if let Some(model_id) = requested_model {
            let model_id = model_id.trim();
            if !model_id.is_empty() {
                if let Some(entry) = self.registry.find(target.provider, model_id)
                    && self.resolve_api_key_with_source(&entry).is_some()
                {
                    return Some(entry);
                }
                return None;
            }
        }

        for model_id in target.preferred_models {
            if let Some(entry) = self.registry.find(target.provider, model_id)
                && self.resolve_api_key_with_source(&entry).is_some()
            {
                return Some(entry);
            }
        }

        self.registry
            .get_available()
            .into_iter()
            .find(|entry| entry.model.provider == target.provider)
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct LiveHttpTrace {
    pub request_url: Option<String>,
    pub request_headers: Vec<(String, String)>,
    pub request_body_bytes: Option<usize>,
    pub response_status: Option<u16>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct LiveStreamSummary {
    pub event_count: usize,
    pub text_chars: usize,
    pub thinking_chars: usize,
    pub tool_calls: usize,
    pub stop_reason: Option<String>,
    pub usage: Usage,
    pub stream_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LiveProviderRun {
    pub provider: String,
    pub model: Option<String>,
    pub api: Option<String>,
    pub status: String,
    pub skip_reason: Option<String>,
    pub error: Option<String>,
    pub elapsed_ms: u64,
    pub response_status: Option<u16>,
    pub request_url: Option<String>,
    pub request_headers: Vec<(String, String)>,
    pub request_body_bytes: Option<usize>,
    pub event_count: usize,
    pub text_chars: usize,
    pub thinking_chars: usize,
    pub tool_calls: usize,
    pub stop_reason: Option<String>,
    pub usage: Usage,
    pub attempts: u32,
    pub retry_backoff_ms: Vec<u64>,
    pub credential_source: Option<String>,
    pub execution_mode: String,
    pub replay_boundary: String,
    pub trace_origin: String,
}

#[must_use]
pub fn ci_e2e_tests_enabled() -> bool {
    env::var(LIVE_E2E_GATE_ENV).is_ok_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes"
        )
    })
}

#[must_use]
pub fn parse_http_status(error_message: &str) -> Option<u16> {
    let marker = "HTTP ";
    let start = error_message.find(marker)? + marker.len();
    let mut digits = String::new();
    for ch in error_message[start..].chars() {
        if ch.is_ascii_digit() {
            digits.push(ch);
        } else {
            break;
        }
    }
    if digits.is_empty() {
        return None;
    }
    digits.parse::<u16>().ok()
}

pub fn create_anthropic_provider(entry: &ModelEntry, client: Client) -> Arc<dyn Provider> {
    Arc::new(
        AnthropicProvider::new(entry.model.id.clone())
            .with_base_url(entry.model.base_url.clone())
            .with_client(client),
    )
}

fn create_openai_completions_provider(entry: &ModelEntry, client: Client) -> Arc<dyn Provider> {
    Arc::new(
        OpenAIProvider::new(entry.model.id.clone())
            .with_provider_name(entry.model.provider.clone())
            .with_base_url(normalize_openai_base(&entry.model.base_url))
            .with_client(client),
    )
}

fn create_openai_responses_provider(entry: &ModelEntry, client: Client) -> Arc<dyn Provider> {
    Arc::new(
        OpenAIResponsesProvider::new(entry.model.id.clone())
            .with_provider_name(entry.model.provider.clone())
            .with_base_url(normalize_openai_responses_base(&entry.model.base_url))
            .with_client(client),
    )
}

pub fn create_openai_provider(
    entry: &ModelEntry,
    client: Client,
) -> pi::PiResult<Arc<dyn Provider>> {
    match entry.model.api.as_str() {
        "openai-completions" => Ok(create_openai_completions_provider(entry, client)),
        "openai-responses" => Ok(create_openai_responses_provider(entry, client)),
        other => Err(pi::Error::provider(
            &entry.model.provider,
            format!("Unsupported OpenAI-compatible API for live harness: {other}"),
        )),
    }
}

pub fn create_gemini_provider(entry: &ModelEntry, client: Client) -> Arc<dyn Provider> {
    Arc::new(
        GeminiProvider::new(entry.model.id.clone())
            .with_base_url(entry.model.base_url.clone())
            .with_client(client),
    )
}

pub fn create_openrouter_provider(
    entry: &ModelEntry,
    client: Client,
) -> pi::PiResult<Arc<dyn Provider>> {
    create_openai_provider(entry, client)
}

pub fn create_xai_provider(entry: &ModelEntry, client: Client) -> pi::PiResult<Arc<dyn Provider>> {
    create_openai_provider(entry, client)
}

pub fn create_deepseek_provider(
    entry: &ModelEntry,
    client: Client,
) -> pi::PiResult<Arc<dyn Provider>> {
    create_openai_provider(entry, client)
}

pub fn create_live_provider(entry: &ModelEntry, client: Client) -> pi::PiResult<Arc<dyn Provider>> {
    match entry.model.provider.as_str() {
        "anthropic" => Ok(create_anthropic_provider(entry, client)),
        "openai" => create_openai_provider(entry, client),
        "google" => Ok(create_gemini_provider(entry, client)),
        "openrouter" => create_openrouter_provider(entry, client),
        "xai" => create_xai_provider(entry, client),
        "deepseek" => create_deepseek_provider(entry, client),
        _ => match entry.model.api.as_str() {
            "anthropic-messages" => Ok(create_anthropic_provider(entry, client)),
            "openai-completions" | "openai-responses" => create_openai_provider(entry, client),
            "google-generative-ai" => Ok(create_gemini_provider(entry, client)),
            other => Err(pi::Error::provider(
                &entry.model.provider,
                format!("Provider not implemented for live harness (api: {other})"),
            )),
        },
    }
}

#[must_use]
pub fn build_live_context(prompt: &str) -> Context<'static> {
    Context::owned(
        Some(
            "You are a deterministic test harness model. Follow the user instruction exactly."
                .to_string(),
        ),
        vec![Message::User(UserMessage {
            content: UserContent::Text(prompt.to_string()),
            timestamp: 0,
        })],
        Vec::new(),
    )
}

#[must_use]
pub fn build_live_stream_options(entry: &ModelEntry, api_key: String) -> StreamOptions {
    let headers: HashMap<String, String> = entry.headers.clone();
    StreamOptions {
        api_key: Some(api_key),
        headers,
        max_tokens: Some(64),
        temperature: Some(0.0),
        thinking_level: Some(ThinkingLevel::Off),
        ..Default::default()
    }
}

#[must_use]
pub fn load_vcr_trace(cassette_path: &Path) -> Option<LiveHttpTrace> {
    let content = std::fs::read_to_string(cassette_path).ok()?;
    let cassette: Cassette = serde_json::from_str(&content).ok()?;
    let interaction = cassette.interactions.last()?;
    let request_body_bytes = interaction
        .request
        .body
        .as_ref()
        .and_then(|value| serde_json::to_vec(value).ok().map(|v| v.len()))
        .or_else(|| interaction.request.body_text.as_ref().map(String::len));

    Some(LiveHttpTrace {
        request_url: Some(interaction.request.url.clone()),
        request_headers: interaction.request.headers.clone(),
        request_body_bytes,
        response_status: Some(interaction.response.status),
    })
}

async fn collect_live_stream_summary(
    provider: Arc<dyn Provider>,
    context: Context<'static>,
    options: StreamOptions,
    timeout: Duration,
) -> Result<LiveStreamSummary, String> {
    let now = asupersync::Cx::current()
        .and_then(|cx| cx.timer_driver())
        .map_or_else(asupersync::time::wall_now, |timer| timer.now());
    let timeout_fut = asupersync::time::sleep(now, timeout).fuse();
    let run_fut = async move {
        let stream = provider
            .stream(&context, &options)
            .await
            .map_err(|err| err.to_string())?;

        let mut summary = LiveStreamSummary::default();
        let mut stream = std::pin::pin!(stream);

        while let Some(item) = stream.next().await {
            match item {
                Ok(event) => {
                    summary.event_count = summary.event_count.saturating_add(1);
                    let terminal =
                        matches!(event, StreamEvent::Done { .. } | StreamEvent::Error { .. });

                    match event {
                        StreamEvent::TextDelta { delta, .. } => {
                            summary.text_chars =
                                summary.text_chars.saturating_add(delta.chars().count());
                        }
                        StreamEvent::TextEnd { content, .. } => {
                            summary.text_chars = content.chars().count();
                        }
                        StreamEvent::ThinkingDelta { delta, .. } => {
                            summary.thinking_chars =
                                summary.thinking_chars.saturating_add(delta.chars().count());
                        }
                        StreamEvent::ThinkingEnd { content, .. } => {
                            summary.thinking_chars = content.chars().count();
                        }
                        StreamEvent::ToolCallEnd { .. } => {
                            summary.tool_calls = summary.tool_calls.saturating_add(1);
                        }
                        StreamEvent::Done { reason, message } => {
                            summary.stop_reason = Some(format!("{reason:?}"));
                            summary.usage = message.usage;
                        }
                        StreamEvent::Error { reason, error } => {
                            summary.stop_reason = Some(format!("{reason:?}"));
                            summary.usage = error.usage;
                            summary.stream_error = error
                                .error_message
                                .or_else(|| Some("provider emitted error event".to_string()));
                        }
                        StreamEvent::Start { .. }
                        | StreamEvent::TextStart { .. }
                        | StreamEvent::ThinkingStart { .. }
                        | StreamEvent::ToolCallStart { .. }
                        | StreamEvent::ToolCallDelta { .. } => {}
                    }

                    if terminal {
                        break;
                    }
                }
                Err(err) => {
                    summary.stream_error = Some(err.to_string());
                    break;
                }
            }
        }

        Ok(summary)
    }
    .fuse();

    pin_mut!(timeout_fut, run_fut);
    match futures::future::select(run_fut, timeout_fut).await {
        futures::future::Either::Left((result, _)) => result,
        futures::future::Either::Right(_) => {
            Err(format!("request timed out after {}s", timeout.as_secs()))
        }
    }
}

#[allow(clippy::too_many_lines)]
pub async fn run_live_provider_target(
    harness: &TestHarness,
    registry: &LiveE2eRegistry,
    target: &LiveProviderTarget,
    vcr_dir: &Path,
) -> LiveProviderRun {
    let start = Instant::now();
    let requested_model = env::var(target.model_env_var)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());

    let Some(entry) = registry.select_entry(target, requested_model.as_deref()) else {
        return LiveProviderRun {
            provider: target.provider.to_string(),
            model: requested_model,
            api: None,
            status: "skipped".to_string(),
            skip_reason: Some(format!(
                "no model with API key for provider '{}'",
                target.provider
            )),
            error: None,
            elapsed_ms: 0,
            response_status: None,
            request_url: None,
            request_headers: Vec::new(),
            request_body_bytes: None,
            event_count: 0,
            text_chars: 0,
            thinking_chars: 0,
            tool_calls: 0,
            stop_reason: None,
            usage: Usage::default(),
            attempts: 0,
            retry_backoff_ms: Vec::new(),
            credential_source: None,
            execution_mode: LIVE_E2E_EXECUTION_MODE.to_string(),
            replay_boundary: LIVE_E2E_REPLAY_BOUNDARY.to_string(),
            trace_origin: LIVE_E2E_TRACE_ORIGIN.to_string(),
        };
    };

    let Some((api_key, credential_source)) = registry.resolve_api_key_with_source(&entry) else {
        return LiveProviderRun {
            provider: target.provider.to_string(),
            model: Some(entry.model.id.clone()),
            api: Some(entry.model.api.clone()),
            status: "skipped".to_string(),
            skip_reason: Some("API key missing".to_string()),
            error: None,
            elapsed_ms: 0,
            response_status: None,
            request_url: None,
            request_headers: Vec::new(),
            request_body_bytes: None,
            event_count: 0,
            text_chars: 0,
            thinking_chars: 0,
            tool_calls: 0,
            stop_reason: None,
            usage: Usage::default(),
            attempts: 0,
            retry_backoff_ms: Vec::new(),
            credential_source: None,
            execution_mode: LIVE_E2E_EXECUTION_MODE.to_string(),
            replay_boundary: LIVE_E2E_REPLAY_BOUNDARY.to_string(),
            trace_origin: LIVE_E2E_TRACE_ORIGIN.to_string(),
        };
    };

    let mut attempt_count = 0u32;
    let mut retry_backoff_ms = Vec::new();
    let mut final_trace = LiveHttpTrace::default();
    let mut final_summary = LiveStreamSummary::default();
    let mut final_error: Option<String> = None;
    let mut final_response_status: Option<u16> = None;
    let mut final_auth_diagnostic: Option<pi::error::AuthDiagnostic> = None;
    let mut final_status = "failed".to_string();

    for attempt in 1..=LIVE_E2E_MAX_ATTEMPTS {
        attempt_count = u32::try_from(attempt).unwrap_or(u32::MAX);
        let cassette_name = format!(
            "live-e2e-{}-{}-attempt-{}",
            entry.model.provider, entry.model.id, attempt
        );
        let recorder = VcrRecorder::new_with(&cassette_name, VcrMode::Record, vcr_dir);
        let cassette_path = recorder.cassette_path().to_path_buf();
        let client = Client::new().with_vcr(recorder);

        harness
            .log()
            .info_ctx("live_e2e", "Invoking live provider call", |ctx| {
                ctx.push(("provider".into(), entry.model.provider.clone()));
                ctx.push(("model".into(), entry.model.id.clone()));
                ctx.push(("api".into(), entry.model.api.clone()));
                ctx.push(("attempt".into(), attempt.to_string()));
                ctx.push(("max_attempts".into(), LIVE_E2E_MAX_ATTEMPTS.to_string()));
                ctx.push(("timeout_s".into(), LIVE_E2E_TIMEOUT.as_secs().to_string()));
                ctx.push(("prompt".into(), target.prompt.to_string()));
                ctx.push(("credential_source".into(), credential_source.clone()));
                ctx.push(("execution_mode".into(), LIVE_E2E_EXECUTION_MODE.to_string()));
                ctx.push((
                    "replay_boundary".into(),
                    LIVE_E2E_REPLAY_BOUNDARY.to_string(),
                ));
                ctx.push(("trace_origin".into(), LIVE_E2E_TRACE_ORIGIN.to_string()));
                ctx.push(("vcr_path".into(), cassette_path.display().to_string()));
            });

        let provider = match create_live_provider(&entry, client) {
            Ok(provider) => provider,
            Err(err) => {
                final_error = Some(format!("provider construction failed: {err}"));
                final_status = "failed".to_string();
                break;
            }
        };

        let context = build_live_context(target.prompt);
        let options = build_live_stream_options(&entry, api_key.clone());
        let summary_result =
            collect_live_stream_summary(provider, context, options, LIVE_E2E_TIMEOUT).await;
        let mut trace = load_vcr_trace(&cassette_path).unwrap_or_default();
        trace.request_headers = redact_sensitive_header_pairs(trace.request_headers);

        let response_status = trace.response_status.or_else(|| {
            summary_result
                .as_ref()
                .err()
                .and_then(|message| parse_http_status(message))
        });
        let summary = summary_result.clone().unwrap_or_default();
        let summary_error = summary_result
            .err()
            .or_else(|| summary.stream_error.clone());
        let attempt_auth_diagnostic = summary_error.as_ref().and_then(|error_message| {
            pi::Error::provider(entry.model.provider.as_str(), error_message.as_str())
                .auth_diagnostic()
        });
        let http_failure = response_status.is_some_and(|status| !(200..300).contains(&status));
        let has_failure = summary_error.is_some() || http_failure;

        harness
            .log()
            .info_ctx("live_e2e", "Provider attempt completed", |ctx| {
                ctx.push(("provider".into(), entry.model.provider.clone()));
                ctx.push(("model".into(), entry.model.id.clone()));
                ctx.push(("attempt".into(), attempt.to_string()));
                ctx.push((
                    "status".into(),
                    if has_failure { "failed" } else { "passed" }.to_string(),
                ));
                if let Some(status) = response_status {
                    ctx.push(("response_status".into(), status.to_string()));
                }
                if let Some(url) = &trace.request_url {
                    ctx.push(("request_url".into(), url.clone()));
                }
                if let Some(bytes) = trace.request_body_bytes {
                    ctx.push(("request_body_bytes".into(), bytes.to_string()));
                }
                ctx.push(("events".into(), summary.event_count.to_string()));
                ctx.push(("tool_calls".into(), summary.tool_calls.to_string()));
                ctx.push(("text_chars".into(), summary.text_chars.to_string()));
                ctx.push(("thinking_chars".into(), summary.thinking_chars.to_string()));
                ctx.push(("usage_input".into(), summary.usage.input.to_string()));
                ctx.push(("usage_output".into(), summary.usage.output.to_string()));
                ctx.push(("usage_total".into(), summary.usage.total_tokens.to_string()));
                if let Some(reason) = &summary.stop_reason {
                    ctx.push(("stop_reason".into(), reason.clone()));
                }
                if let Some(error) = &summary_error {
                    ctx.push(("error".into(), error.clone()));
                }
                if let Some(diagnostic) = attempt_auth_diagnostic {
                    ctx.push((
                        "diagnostic_code".into(),
                        diagnostic.code.as_str().to_string(),
                    ));
                    ctx.push((
                        "diagnostic_remediation".into(),
                        diagnostic.remediation.to_string(),
                    ));
                    ctx.push((
                        "redaction_policy".into(),
                        diagnostic.redaction_policy.to_string(),
                    ));
                }
            });

        final_trace = trace;
        final_summary = summary;
        final_error = summary_error.clone();
        final_response_status = response_status;
        final_auth_diagnostic = attempt_auth_diagnostic;
        final_status = if has_failure {
            "failed".to_string()
        } else {
            "passed".to_string()
        };

        if !has_failure {
            break;
        }

        if should_retry_live_attempt(attempt, response_status, summary_error.as_deref()) {
            let backoff = retry_backoff_for_attempt(attempt);
            let backoff_ms = u64::try_from(backoff.as_millis()).unwrap_or(u64::MAX);
            retry_backoff_ms.push(backoff_ms);

            harness.log().with_context(
                LogLevel::Warn,
                "live_e2e",
                "Transient failure; scheduling retry",
                |ctx| {
                    ctx.push(("provider".into(), entry.model.provider.clone()));
                    ctx.push(("model".into(), entry.model.id.clone()));
                    ctx.push(("attempt".into(), attempt.to_string()));
                    ctx.push(("next_attempt".into(), (attempt + 1).to_string()));
                    ctx.push(("backoff_ms".into(), backoff_ms.to_string()));
                    if let Some(status) = response_status {
                        ctx.push(("response_status".into(), status.to_string()));
                    }
                    if let Some(error) = &summary_error {
                        ctx.push(("error".into(), error.clone()));
                    }
                    if let Some(diagnostic) = attempt_auth_diagnostic {
                        ctx.push((
                            "diagnostic_code".into(),
                            diagnostic.code.as_str().to_string(),
                        ));
                    }
                },
            );

            asupersync::time::sleep(asupersync::time::wall_now(), backoff).await;
            continue;
        }

        break;
    }

    let elapsed_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
    harness
        .log()
        .info_ctx("live_e2e", "Provider call completed", |ctx| {
            ctx.push(("provider".into(), entry.model.provider.clone()));
            ctx.push(("model".into(), entry.model.id.clone()));
            ctx.push(("status".into(), final_status.clone()));
            ctx.push(("attempts".into(), attempt_count.to_string()));
            ctx.push(("elapsed_ms".into(), elapsed_ms.to_string()));
            ctx.push(("credential_source".into(), credential_source.clone()));
            ctx.push(("execution_mode".into(), LIVE_E2E_EXECUTION_MODE.to_string()));
            ctx.push((
                "replay_boundary".into(),
                LIVE_E2E_REPLAY_BOUNDARY.to_string(),
            ));
            ctx.push(("trace_origin".into(), LIVE_E2E_TRACE_ORIGIN.to_string()));
            if !retry_backoff_ms.is_empty() {
                ctx.push(("retry_backoff_ms".into(), format!("{retry_backoff_ms:?}")));
            }
            if let Some(status) = final_response_status {
                ctx.push(("response_status".into(), status.to_string()));
            }
            if let Some(error) = &final_error {
                ctx.push(("error".into(), error.clone()));
            }
            if let Some(diagnostic) = final_auth_diagnostic {
                ctx.push((
                    "diagnostic_code".into(),
                    diagnostic.code.as_str().to_string(),
                ));
                ctx.push((
                    "diagnostic_remediation".into(),
                    diagnostic.remediation.to_string(),
                ));
                ctx.push((
                    "redaction_policy".into(),
                    diagnostic.redaction_policy.to_string(),
                ));
            }
        });

    LiveProviderRun {
        provider: entry.model.provider.clone(),
        model: Some(entry.model.id.clone()),
        api: Some(entry.model.api.clone()),
        status: final_status,
        skip_reason: None,
        error: final_error,
        elapsed_ms,
        response_status: final_response_status,
        request_url: final_trace.request_url,
        request_headers: final_trace.request_headers,
        request_body_bytes: final_trace.request_body_bytes,
        event_count: final_summary.event_count,
        text_chars: final_summary.text_chars,
        thinking_chars: final_summary.thinking_chars,
        tool_calls: final_summary.tool_calls,
        stop_reason: final_summary.stop_reason,
        usage: final_summary.usage,
        attempts: attempt_count,
        retry_backoff_ms,
        credential_source: Some(credential_source),
        execution_mode: LIVE_E2E_EXECUTION_MODE.to_string(),
        replay_boundary: LIVE_E2E_REPLAY_BOUNDARY.to_string(),
        trace_origin: LIVE_E2E_TRACE_ORIGIN.to_string(),
    }
}

pub fn write_live_provider_runs_jsonl(
    harness: &TestHarness,
    file_name: &str,
    runs: &[LiveProviderRun],
) -> std::io::Result<PathBuf> {
    let path = harness.temp_path(file_name);
    let mut content = String::new();
    for run in runs {
        let line = serde_json::to_string(run)
            .unwrap_or_else(|_| "{\"status\":\"serialization_error\"}".to_string());
        content.push_str(&line);
        content.push('\n');
    }
    std::fs::write(&path, content)?;
    harness.record_artifact(file_name.to_string(), &path);
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpStream;

    #[test]
    fn test_harness_basic() {
        let harness = TestHarness::new("basic_test");

        // Verify temp dir exists
        assert!(harness.temp_dir().exists());

        // Create and verify file
        let path = harness.create_file("test.txt", "hello world");
        assert!(path.exists());
        assert_eq!(harness.read_file("test.txt"), "hello world");
    }

    #[test]
    fn test_harness_nested_files() {
        let harness = TestHarness::new("nested_test");

        // Create nested file
        let path = harness.create_file("subdir/deep/test.txt", "nested content");
        assert!(path.exists());
        assert_eq!(harness.read_file("subdir/deep/test.txt"), "nested content");
    }

    #[test]
    fn test_harness_logging() {
        let harness = TestHarness::new("logging_test");

        harness.log().info("test", "Custom log message");
        harness.section("Phase 1");
        harness.assert_log("Checking something");

        assert!(harness.log().entry_count() > 0);
    }

    #[test]
    fn test_builder() {
        let harness = TestHarnessBuilder::new("builder_test")
            .no_colors()
            .min_level(LogLevel::Info)
            .build();

        harness.log().debug("test", "Should be filtered");
        harness.log().info("test", "Should appear");

        // Debug should be filtered out
        let entries = harness.log().entries();
        let debug_count = entries
            .iter()
            .filter(|e| e.level == LogLevel::Debug)
            .count();
        assert_eq!(debug_count, 0);
    }

    #[test]
    fn test_mock_http_server_records_requests_and_redacts() {
        let harness = TestHarness::new("mock_http_server_records_requests_and_redacts");
        let server = harness.start_mock_http_server();
        server.add_route("GET", "/hello", MockHttpResponse::text(200, "world"));

        let mut stream = TcpStream::connect(server.addr()).expect("connect mock server");
        stream
            .write_all(
                b"GET /hello HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer secret-token\r\n\r\n",
            )
            .expect("write request");
        stream.flush().expect("flush request");

        let mut response = String::new();
        stream.read_to_string(&mut response).expect("read response");
        let _ = stream.shutdown(std::net::Shutdown::Both);

        assert!(response.starts_with("HTTP/1.1 200"));
        assert!(response.contains("world"));

        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "GET");
        assert_eq!(requests[0].path, "/hello");

        // Ensure the logger redacted the sensitive header value.
        let dump = harness.log().dump();
        assert!(dump.contains("header.authorization = [REDACTED]"));
        assert!(!dump.contains("secret-token"));
    }

    #[test]
    fn test_provider_api_key_env_vars_use_metadata_aliases() {
        assert_eq!(
            provider_api_key_env_vars("openrouter"),
            &["OPENROUTER_API_KEY"]
        );
        assert_eq!(provider_api_key_env_vars("xai"), &["XAI_API_KEY"]);
        assert_eq!(provider_api_key_env_vars("deepseek"), &["DEEPSEEK_API_KEY"]);
        assert_eq!(
            provider_api_key_env_vars("dashscope"),
            &["DASHSCOPE_API_KEY", "QWEN_API_KEY"]
        );
        assert_eq!(
            provider_api_key_env_vars("kimi"),
            &["MOONSHOT_API_KEY", "KIMI_API_KEY"]
        );
    }
}
