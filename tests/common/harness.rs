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
use sha2::{Digest, Sha256};
use std::env;
use std::fmt::Write as _;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, mpsc};
use std::thread::{self, JoinHandle};
use tempfile::TempDir;

/// Test harness providing temp directories, logging, and cleanup.
pub struct TestHarness {
    /// Test name for identification in logs.
    name: String,
    /// Temporary directory for test files.
    temp_dir: TempDir,
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
        let logger = Arc::new(TestLogger::new());
        logger.set_test_name(&name);
        logger.set_normalization_root(temp_dir.path());

        logger
            .as_ref()
            .info("harness", format!("Test '{name}' started"));
        logger
            .as_ref()
            .info_ctx("harness", "Temp directory created", |ctx| {
                ctx.push(("path".into(), temp_dir.path().display().to_string()));
            });

        Self {
            name,
            temp_dir,
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

    /// Get the path to the temporary directory.
    pub fn temp_dir(&self) -> &Path {
        self.temp_dir.path()
    }

    /// Get a path within the temporary directory.
    ///
    /// This is a convenience method that joins the given path to the temp directory.
    pub fn temp_path(&self, path: impl AsRef<Path>) -> PathBuf {
        self.temp_dir.path().join(path)
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

    /// Manually dump logs (useful for debugging passing tests).
    pub fn dump_logs(&self) {
        let header = format!("\n=== TEST LOGS: {} ===\n", self.name);
        if self.use_colors {
            eprint!("\x1b[1;36m{header}\x1b[0m");
            eprint!("{}", self.logger.dump_colored());
        } else {
            eprint!("{header}");
            eprint!("{}", self.logger.dump());
        }
        if self.logger.has_artifacts() {
            eprintln!("=== ARTIFACTS ===");
            eprint!("{}", self.logger.dump_artifacts());
            eprintln!("=== END ARTIFACTS ===");
        }
        eprintln!("=== END LOGS ===\n");
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

            if let Ok(path) = env::var("TEST_LOG_PATH") {
                if let Err(err) = self.logger.as_ref().write_dump_to_path(&path) {
                    eprintln!("Failed to write test log to {path}: {err}");
                }
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
        let logger = Arc::new(TestLogger::with_min_level(self.min_log_level));
        let name = self.name;
        logger.set_test_name(&name);
        logger.set_normalization_root(temp_dir.path());

        logger
            .as_ref()
            .info("harness", format!("Test '{name}' started"));
        logger
            .as_ref()
            .info_ctx("harness", "Temp directory created", |ctx| {
                ctx.push(("path".into(), temp_dir.path().display().to_string()));
            });

        TestHarness {
            name,
            temp_dir,
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
        let requests = Arc::new(Mutex::new(Vec::new()));
        let shutdown = Arc::new(AtomicBool::new(false));

        let thread_routes = Arc::clone(&routes);
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

    #[must_use]
    pub fn requests(&self) -> Vec<MockHttpRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for MockHttpServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);

        // Best-effort: poke the listener to unblock accept loops on some platforms.
        let _ = TcpStream::connect(self.addr);

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
        let n = stream.read(scratch)?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "connection closed before request headers",
            ));
        }
        buf.extend_from_slice(&scratch[..n]);
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

    let response = routes
        .lock()
        .unwrap()
        .get(&RouteKey { method, path })
        .cloned()
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
}
